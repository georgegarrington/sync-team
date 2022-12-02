use anyhow::bail;
use hyper_old_types::header::{Link, RelationType};
use log::{debug, trace};
use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    header::{self, HeaderValue},
    Method, StatusCode,
};
use serde::de::DeserializeOwned;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

pub(crate) struct GitHub {
    token: String,
    dry_run: bool,
    client: Client,
}

impl GitHub {
    pub(crate) fn new(token: String, dry_run: bool) -> Self {
        GitHub {
            token,
            dry_run,
            client: Client::new(),
        }
    }

    /// Get user names by user ids
    pub(crate) fn usernames(&self, ids: &[usize]) -> anyhow::Result<HashMap<usize, String>> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Usernames {
            database_id: usize,
            login: String,
        }
        #[derive(serde::Serialize)]
        struct Params {
            ids: Vec<String>,
        }
        static QUERY: &str = "
            query($ids: [ID!]!) {
                nodes(ids: $ids) {
                    ... on User {
                        databaseId
                        login
                    }
                }
            }
        ";

        let mut result = HashMap::new();
        for chunk in ids.chunks(100) {
            let res: GraphNodes<Usernames> = self.graphql(
                QUERY,
                Params {
                    ids: chunk.iter().map(|id| user_node_id(*id)).collect(),
                },
            )?;
            for node in res.nodes.into_iter().flatten() {
                result.insert(node.database_id, node.login);
            }
        }
        Ok(result)
    }

    /// Get the owners of an org
    pub(crate) fn org_owners(&self, org: &str) -> anyhow::Result<HashSet<usize>> {
        #[derive(serde::Deserialize, Eq, PartialEq, Hash)]
        struct User {
            id: usize,
        }
        let mut owners = HashSet::new();
        self.rest_paginated(
            &Method::GET,
            format!("orgs/{org}/members?role=admin"),
            |resp| {
                let partial: Vec<User> = resp.json()?;
                for owner in partial {
                    owners.insert(owner.id);
                }
                Ok(())
            },
        )?;
        Ok(owners)
    }

    /// Get all teams associated with a org
    pub(crate) fn org_teams(&self, org: &str) -> anyhow::Result<HashSet<String>> {
        let mut teams = HashSet::new();

        self.rest_paginated(&Method::GET, format!("orgs/{org}/teams"), |resp| {
            let partial: Vec<Team> = resp.json()?;
            for team in partial {
                teams.insert(team.name);
            }
            Ok(())
        })?;

        Ok(teams)
    }

    /// Get the team by name and org
    pub(crate) fn team(&self, org: &str, team: &str) -> anyhow::Result<Option<Team>> {
        self.send_option(Method::GET, &format!("orgs/{}/teams/{}", org, team))
    }

    /// Create a team in a org
    pub(crate) fn create_team(
        &self,
        org: &str,
        name: &str,
        description: &str,
        privacy: TeamPrivacy,
    ) -> anyhow::Result<Team> {
        #[derive(serde::Serialize, Debug)]
        struct Req<'a> {
            name: &'a str,
            description: &'a str,
            privacy: TeamPrivacy,
        }
        debug!("Creating team '{name}' in '{org}'");
        if self.dry_run {
            Ok(Team {
                // The `None` marks that the team is "created" by the dry run and
                // doesn't actually exist on GitHub
                id: None,
                name: name.to_string(),
                description: description.to_string(),
                privacy,
            })
        } else {
            let body = &Req {
                name,
                description,
                privacy,
            };
            Ok(self
                .send(Method::POST, &format!("orgs/{}/teams", org), body)?
                .json()?)
        }
    }

    /// Edit a team
    pub(crate) fn edit_team(
        &self,
        org: &str,
        name: &str,
        new_name: Option<&str>,
        new_description: Option<&str>,
        new_privacy: Option<TeamPrivacy>,
    ) -> anyhow::Result<()> {
        #[derive(serde::Serialize, Debug)]
        struct Req<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            description: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            privacy: Option<TeamPrivacy>,
        }
        let req = Req {
            name: new_name,
            description: new_description,
            privacy: new_privacy,
        };
        debug!("Editing team '{name}' in '{org}' with request: {req:?}");
        if !self.dry_run {
            self.send(Method::PATCH, &format!("orgs/{org}/teams/{name}"), &req)?;
        }

        Ok(())
    }

    /// Delete a team by name and org
    pub(crate) fn delete_team(&self, org: &str, team: &str) -> anyhow::Result<()> {
        debug!("Deleting team '{team}' in '{org}'");
        if !self.dry_run {
            self.req(Method::DELETE, &format!("orgs/{}/teams/{}", org, team))?
                .send()?
                .error_for_status()?;
        }
        Ok(())
    }

    pub(crate) fn team_memberships(
        &self,
        team: &Team,
    ) -> anyhow::Result<HashMap<usize, TeamMember>> {
        #[derive(serde::Deserialize)]
        struct RespTeam {
            members: RespMembers,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RespMembers {
            page_info: GraphPageInfo,
            edges: Vec<RespEdge>,
        }
        #[derive(serde::Deserialize)]
        struct RespEdge {
            role: TeamRole,
            node: RespNode,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RespNode {
            database_id: usize,
            login: String,
        }
        #[derive(serde::Serialize)]
        struct Params<'a> {
            team: String,
            cursor: Option<&'a str>,
        }
        static QUERY: &str = "
            query($team: ID!, $cursor: String) {
                node(id: $team) {
                    ... on Team {
                        members(after: $cursor) {
                            pageInfo {
                                endCursor
                                hasNextPage
                            }
                            edges {
                                role
                                node {
                                    databaseId
                                    login
                                }
                            }
                        }
                    }
                }
            }
        ";

        let mut memberships = HashMap::new();
        // Return the empty HashMap on new teams from dry runs
        if let Some(id) = team.id {
            let mut page_info = GraphPageInfo::start();
            while page_info.has_next_page {
                let res: GraphNode<RespTeam> = self.graphql(
                    QUERY,
                    Params {
                        team: team_node_id(id),
                        cursor: page_info.end_cursor.as_deref(),
                    },
                )?;
                if let Some(team) = res.node {
                    page_info = team.members.page_info;
                    for edge in team.members.edges.into_iter() {
                        memberships.insert(
                            edge.node.database_id,
                            TeamMember {
                                username: edge.node.login,
                                role: edge.role,
                            },
                        );
                    }
                }
            }
        }

        Ok(memberships)
    }

    /// Set a user's membership in a team to a role
    pub(crate) fn set_team_membership(
        &self,
        org: &str,
        team: &str,
        user: &str,
        role: TeamRole,
    ) -> anyhow::Result<()> {
        debug!("Setting membership of '{user}' in team '{team}' to {role} in '{org}'");
        #[derive(serde::Serialize, Debug)]
        struct Req {
            role: TeamRole,
        }
        if !self.dry_run {
            self.send(
                Method::PUT,
                &format!("orgs/{org}/teams/{team}/memberships/{user}"),
                &Req { role },
            )?;
        }

        Ok(())
    }

    /// Remove a user from a team
    pub(crate) fn remove_team_membership(
        &self,
        org: &str,
        team: &str,
        user: &str,
    ) -> anyhow::Result<()> {
        debug!("Removing membership of '{user}' from team '{team}' in '{org}'");
        if !self.dry_run {
            self.req(
                Method::DELETE,
                &format!("orgs/{org}/teams/{team}/memberships/{user}"),
            )?
            .send()?
            .error_for_status()?;
        }

        Ok(())
    }

    /// Get a repo by org and name
    pub(crate) fn repo(&self, org: &str, repo: &str) -> anyhow::Result<Option<Repo>> {
        self.send_option(Method::GET, &format!("repos/{org}/{repo}"))
    }

    /// Create a repo
    pub(crate) fn create_repo(
        &self,
        org: &str,
        name: &str,
        description: &str,
    ) -> anyhow::Result<Repo> {
        #[derive(serde::Serialize, Debug)]
        struct Req<'a> {
            name: &'a str,
            description: &'a str,
        }
        let req = &Req { name, description };
        debug!("Creating the repo {org}/{name} with {req:?}");
        if self.dry_run {
            Ok(Repo {
                name: name.to_string(),
                org: org.to_string(),
                description: Some(description.to_string()),
                default_branch: String::from("main"),
            })
        } else {
            Ok(self
                .send(Method::POST, &format!("orgs/{org}/repos"), req)?
                .json()?)
        }
    }

    pub(crate) fn edit_repo(&self, repo: &Repo, description: &str) -> anyhow::Result<()> {
        #[derive(serde::Serialize, Debug)]
        struct Req<'a> {
            description: &'a str,
        }
        let req = Req { description };
        debug!("Editing repo {}/{} with {:?}", repo.org, repo.name, req);
        if !self.dry_run {
            self.send(
                Method::PATCH,
                &format!("repos/{}/{}", repo.org, repo.name),
                &req,
            )?;
        }
        Ok(())
    }

    /// Get teams in a repo
    pub(crate) fn repo_teams(&self, org: &str, repo: &str) -> anyhow::Result<Vec<RepoTeam>> {
        let mut teams = Vec::new();

        self.rest_paginated(&Method::GET, format!("repos/{org}/{repo}/teams"), |resp| {
            let partial: Vec<RepoTeam> = resp.json()?;
            for team in partial {
                teams.push(team);
            }
            Ok(())
        })?;

        Ok(teams)
    }

    /// Get collaborators in a repo
    ///
    /// Only fetches those who are direct collaborators (i.e., not a collaborator through a repo team)
    pub(crate) fn repo_collaborators(
        &self,
        org: &str,
        repo: &str,
    ) -> anyhow::Result<Vec<RepoUser>> {
        let mut users = Vec::new();

        self.rest_paginated(
            &Method::GET,
            format!("repos/{org}/{repo}/collaborators?affiliation=direct"),
            |resp| {
                let partial: Vec<RepoUser> = resp.json()?;
                for user in partial {
                    users.push(user);
                }
                Ok(())
            },
        )?;

        Ok(users)
    }

    /// Update a team's permissions to a repo
    pub(crate) fn update_team_repo_permissions(
        &self,
        org: &str,
        repo: &str,
        team: &str,
        permission: &RepoPermission,
    ) -> anyhow::Result<()> {
        #[derive(serde::Serialize, Debug)]
        struct Req<'a> {
            permission: &'a RepoPermission,
        }
        debug!("Updating permission for team {team} on {org}/{repo} to {permission:?}");
        if !self.dry_run {
            self.send(
                Method::PUT,
                &format!("orgs/{org}/teams/{team}/repos/{org}/{repo}"),
                &Req { permission },
            )?;
        }

        Ok(())
    }

    /// Update a user's permissions to a repo
    pub(crate) fn update_user_repo_permissions(
        &self,
        org: &str,
        repo: &str,
        user: &str,
        permission: &RepoPermission,
    ) -> anyhow::Result<()> {
        #[derive(serde::Serialize, Debug)]
        struct Req<'a> {
            permission: &'a RepoPermission,
        }
        debug!("Updating permission for user {user} on {org}/{repo} to {permission:?}");
        if !self.dry_run {
            self.send(
                Method::PUT,
                &format!("repos/{org}/{repo}/collaborators/{user}"),
                &Req { permission },
            )?;
        }
        Ok(())
    }

    /// Remove a team from a repo
    pub(crate) fn remove_team_from_repo(
        &self,
        org: &str,
        repo: &str,
        team: &str,
    ) -> anyhow::Result<()> {
        debug!("Removing team {team} from repo {org}/{repo}");
        if !self.dry_run {
            self.req(
                Method::DELETE,
                &format!("orgs/{org}/teams/{team}/repos/{org}/{repo}"),
            )?
            .send()?
            .error_for_status()?;
        }

        Ok(())
    }

    /// Remove a collaborator from a repo
    pub(crate) fn remove_collaborator_from_repo(
        &self,
        org: &str,
        repo: &str,
        collaborator: &str,
    ) -> anyhow::Result<()> {
        debug!("Removing collaborator {collaborator} from repo {org}/{repo}");
        if !self.dry_run {
            self.req(
                Method::DELETE,
                &format!("repos/{org}/{repo}/collaborators/{collaborator}"),
            )?
            .send()?
            .error_for_status()?;
        }
        Ok(())
    }

    /// Get the head commit of the supplied branch
    pub(crate) fn branch(&self, repo: &Repo, name: &str) -> anyhow::Result<Option<String>> {
        let branch = self.send_option::<Branch>(
            Method::GET,
            &format!("repos/{}/{}/branches/{}", repo.org, repo.name, name),
        )?;
        Ok(branch.map(|b| b.commit.sha))
    }

    /// Create a branch
    pub(crate) fn create_branch(
        &self,
        repo: &Repo,
        name: &str,
        commit: &str,
    ) -> anyhow::Result<()> {
        #[derive(serde::Serialize, Debug)]
        struct Req<'a> {
            r#ref: &'a str,
            sha: &'a str,
        }
        debug!(
            "Creating branch in {}/{}: {} with commit {}",
            repo.org, repo.name, name, commit
        );
        if !self.dry_run {
            self.send(
                Method::POST,
                &format!("repos/{}/{}/git/refs", repo.org, repo.name),
                &Req {
                    r#ref: &format!("refs/heads/{}", name),
                    sha: commit,
                },
            )?;
        }
        Ok(())
    }

    /// Get protected branches from a repo
    pub(crate) fn protected_branches(&self, repo: &Repo) -> anyhow::Result<HashSet<String>> {
        let mut names = HashSet::new();
        self.rest_paginated(
            &Method::GET,
            format!("repos/{}/{}/branches?protected=true", repo.org, repo.name),
            |resp| {
                let resp = resp.json::<Vec<Branch>>()?;
                names.extend(resp.into_iter().map(|b| b.name));

                Ok(())
            },
        )?;
        Ok(names)
    }

    /// Update a branch's permissions.
    ///
    /// Returns `Ok(true)` on success, `Ok(false)` if the branch doesn't exist, and `Err(_)` otherwise.
    pub(crate) fn update_branch_protection(
        &self,
        repo: &Repo,
        branch_name: &str,
        branch_protection: BranchProtection,
    ) -> anyhow::Result<bool> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            required_status_checks: Req1<'a>,
            enforce_admins: bool,
            required_pull_request_reviews: Req2,
            restrictions: HashMap<String, Vec<String>>,
        }
        #[derive(serde::Serialize)]
        struct Req1<'a> {
            strict: bool,
            checks: Vec<Check<'a>>,
        }
        #[derive(serde::Serialize)]
        struct Check<'a> {
            context: &'a str,
        }
        #[derive(serde::Serialize)]
        struct Req2 {
            // Even though we don't want dismissal restrictions, it cannot be ommited
            dismissal_restrictions: HashMap<(), ()>,
            dismiss_stale_reviews: bool,
            required_approving_review_count: u8,
        }
        let req = Req {
            required_status_checks: Req1 {
                strict: false,
                checks: branch_protection
                    .required_checks
                    .iter()
                    .map(|c| Check {
                        context: c.as_str(),
                    })
                    .collect(),
            },
            enforce_admins: true,
            required_pull_request_reviews: Req2 {
                dismissal_restrictions: HashMap::new(),
                dismiss_stale_reviews: branch_protection.dismiss_stale_reviews,
                required_approving_review_count: branch_protection.required_approving_review_count,
            },
            restrictions: vec![
                ("users".to_string(), branch_protection.allowed_users),
                ("teams".to_string(), Vec::new()),
            ]
            .into_iter()
            .collect(),
        };
        debug!(
            "Updating branch protection on repo {}/{} for {}: {}",
            repo.org,
            repo.name,
            branch_name,
            serde_json::to_string_pretty(&req).unwrap_or_else(|_| "<invalid json>".to_string())
        );
        if !self.dry_run {
            let resp = self
                .req(
                    Method::PUT,
                    &format!(
                        "repos/{}/{}/branches/{}/protection",
                        repo.org, repo.name, branch_name
                    ),
                )?
                .json(&req)
                .send()?;
            match resp.status() {
                StatusCode::OK => Ok(true),
                StatusCode::NOT_FOUND => Ok(false),
                _ => {
                    resp.error_for_status()?;
                    Ok(false)
                }
            }
        } else {
            Ok(true)
        }
    }

    /// Delete a branch protection
    pub(crate) fn delete_branch_protection(&self, repo: &Repo, branch: &str) -> anyhow::Result<()> {
        debug!(
            "Removing protection in {}/{} from {} branch",
            repo.org, repo.name, branch
        );
        if !self.dry_run {
            self.req(
                Method::DELETE,
                &format!(
                    "repos/{}/{}/branches/{}/protection",
                    repo.org, repo.name, branch
                ),
            )?
            .send()?
            .error_for_status()?;
        }
        Ok(())
    }

    fn req(&self, method: Method, url: &str) -> anyhow::Result<RequestBuilder> {
        let url = if url.starts_with("https://") {
            Cow::Borrowed(url)
        } else {
            Cow::Owned(format!("https://api.github.com/{}", url))
        };
        trace!("http request: {} {}", method, url);
        if self.dry_run && method != Method::GET && !url.contains("graphql") {
            panic!("Called a non-GET request in dry run mode: {}", method);
        }
        Ok(self
            .client
            .request(method, url.as_ref())
            .header(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("token {}", self.token))?,
            )
            .header(
                header::USER_AGENT,
                HeaderValue::from_static(crate::USER_AGENT),
            ))
    }

    fn send<T: serde::Serialize + std::fmt::Debug>(
        &self,
        method: Method,
        url: &str,
        body: &T,
    ) -> Result<Response, anyhow::Error> {
        Ok(self
            .req(method, url)?
            .json(body)
            .send()?
            .error_for_status()?)
    }

    fn send_option<T: DeserializeOwned>(
        &self,
        method: Method,
        url: &str,
    ) -> Result<Option<T>, anyhow::Error> {
        let resp = self.req(method, url)?.send()?;
        match resp.status() {
            StatusCode::OK => Ok(Some(resp.json()?)),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(resp.error_for_status().unwrap_err().into()),
        }
    }

    fn graphql<R, V>(&self, query: &str, variables: V) -> anyhow::Result<R>
    where
        R: serde::de::DeserializeOwned,
        V: serde::Serialize,
    {
        #[derive(serde::Serialize)]
        struct Request<'a, V> {
            query: &'a str,
            variables: V,
        }
        let res: GraphResult<R> = self
            .req(Method::POST, "graphql")?
            .json(&Request { query, variables })
            .send()?
            .error_for_status()?
            .json()?;
        if let Some(error) = res.errors.get(0) {
            bail!("graphql error: {}", error.message);
        } else if let Some(data) = res.data {
            Ok(data)
        } else {
            bail!("missing graphql data");
        }
    }

    fn rest_paginated<F>(&self, method: &Method, url: String, mut f: F) -> anyhow::Result<()>
    where
        F: FnMut(Response) -> anyhow::Result<()>,
    {
        let mut next = Some(url);
        while let Some(next_url) = next.take() {
            let resp = self
                .req(method.clone(), &next_url)?
                .send()?
                .error_for_status()?;

            // Extract the next page
            if let Some(links) = resp.headers().get(header::LINK) {
                let links: Link = links.to_str()?.parse()?;
                for link in links.values() {
                    if link
                        .rel()
                        .map(|r| r.iter().any(|r| *r == RelationType::Next))
                        .unwrap_or(false)
                    {
                        next = Some(link.link().to_string());
                        break;
                    }
                }
            }

            f(resp)?;
        }
        Ok(())
    }
}

#[derive(serde::Deserialize)]
struct GraphResult<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphError>,
}

#[derive(serde::Deserialize)]
struct GraphError {
    message: String,
}

#[derive(serde::Deserialize)]
struct GraphNodes<T> {
    nodes: Vec<Option<T>>,
}

#[derive(serde::Deserialize)]
struct GraphNode<T> {
    node: Option<T>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphPageInfo {
    end_cursor: Option<String>,
    has_next_page: bool,
}

impl GraphPageInfo {
    fn start() -> Self {
        GraphPageInfo {
            end_cursor: None,
            has_next_page: true,
        }
    }
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct Team {
    /// The ID returned by the GitHub API can't be empty, but the None marks teams "created" during
    /// a dry run and not actually present on GitHub, so other methods can avoid acting on them.
    pub(crate) id: Option<usize>,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) privacy: TeamPrivacy,
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct RepoTeam {
    pub(crate) name: String,
    pub(crate) permission: RepoPermission,
}

#[derive(serde::Deserialize)]
pub(crate) struct RepoUser {
    #[serde(alias = "login")]
    pub(crate) name: String,
    pub(crate) permission: RepoPermission,
}

#[derive(Copy, Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepoPermission {
    // While the GitHub UI uses the term 'write', the API still uses the older term 'push'
    #[serde(rename = "push")]
    Write,
    Admin,
    Maintain,
    Triage,
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct Repo {
    pub(crate) name: String,
    #[serde(alias = "owner", deserialize_with = "repo_owner")]
    pub(crate) org: String,
    pub(crate) description: Option<String>,
    pub(crate) default_branch: String,
}

fn repo_owner<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    use serde::de::Deserialize;
    let owner = RepoOwner::deserialize(deserializer)?;
    Ok(owner.login)
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct RepoOwner {
    login: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Eq, PartialEq, Copy, Clone)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TeamPrivacy {
    Closed,
    Secret,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Eq, PartialEq, Copy, Clone)]
#[serde(rename_all(serialize = "snake_case", deserialize = "SCREAMING_SNAKE_CASE"))]
pub(crate) enum TeamRole {
    Member,
    Maintainer,
}

impl fmt::Display for TeamRole {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TeamRole::Member => write!(f, "member"),
            TeamRole::Maintainer => write!(f, "maintainer"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TeamMember {
    pub(crate) username: String,
    pub(crate) role: TeamRole,
}

fn user_node_id(id: usize) -> String {
    base64::encode(&format!("04:User{}", id))
}

fn team_node_id(id: usize) -> String {
    base64::encode(&format!("04:Team{}", id))
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct Branch {
    pub(crate) name: String,
    pub(crate) commit: Commit,
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct Commit {
    pub(crate) sha: String,
}

#[derive(Debug)]
pub(crate) struct BranchProtection {
    pub(crate) dismiss_stale_reviews: bool,
    pub(crate) required_approving_review_count: u8,
    pub(crate) required_checks: Vec<String>,
    pub(crate) allowed_users: Vec<String>,
}
