//! Stateful fakes of the GitHub and Forgejo REST APIs.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferry::forge::MARKER_TOPIC;
use serde_json::{Value, json};
use wiremock::{Request, Respond, ResponseTemplate};

use super::git::{git, init_bare, ref_map, write_script};
use super::{Events, FORGEJO_LOGIN, lock};

/// A repository as the fake Forgejo API reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRepo {
    pub private: bool,
    pub mirror: bool,
    pub default_branch: String,
    pub description: String,
    pub has_actions: bool,
    pub topics: Vec<String>,
    /// What the API reports as `empty`. `None` derives it from the refs.
    pub reports_empty: Option<bool>,
}

impl Default for FakeRepo {
    fn default() -> Self {
        Self {
            private: true,
            mirror: false,
            default_branch: "main".to_string(),
            description: String::new(),
            // Forgejo enables the Actions unit on new repositories.
            has_actions: true,
            topics: Vec::new(),
            reports_empty: None,
        }
    }
}

impl FakeRepo {
    pub fn has_marker(&self) -> bool {
        self.topics.iter().any(|topic| topic == MARKER_TOPIC)
    }

    pub fn marked() -> Self {
        Self {
            topics: vec![MARKER_TOPIC.to_string()],
            ..Self::default()
        }
    }

    fn to_json(&self, git_dir: &Path) -> Value {
        json!({
            "private": self.private,
            "mirror": self.mirror,
            "default_branch": self.default_branch,
            "description": self.description,
            "has_actions": self.has_actions,
            "topics": self.topics,
            "empty": self
                .reports_empty
                .unwrap_or_else(|| ref_map(git_dir).is_empty()),
        })
    }
}

/// How the fake GitHub API answers `GET /repos/{owner}/{repo}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubAnswer {
    Description(String),
    Status(u16),
}

#[derive(Debug, Default)]
pub struct ForgeState {
    /// Forgejo repositories keyed by lowercase `owner/name`.
    pub repos: BTreeMap<String, FakeRepo>,
    /// GitHub REST answers keyed by lowercase `owner/name`. Missing → 404.
    pub github: HashMap<String, GithubAnswer>,
    /// When set, `PATCH …/repos/{owner}/{repo}` answers with this status.
    pub edit_status: Option<u16>,
    /// When set, `GET /api/v1/user` answers with this status.
    pub whoami_status: Option<u16>,
}

/// Key of a repository in `ForgeState`: lowercase `owner/name`.
pub fn repo_key(owner: &str, name: &str) -> String {
    format!("{owner}/{name}").to_lowercase()
}

pub struct ForgejoApi {
    pub root: PathBuf,
    pub state: Arc<Mutex<ForgeState>>,
    pub events: Events,
}

impl ForgejoApi {
    fn git_dir(&self, owner: &str, name: &str) -> PathBuf {
        forgejo_git_dir(&self.root, owner, name)
    }

    fn create(&self, owner: &str, body: &Value) -> ResponseTemplate {
        let name = body["name"].as_str().unwrap_or_default().to_string();
        let key = repo_key(owner, &name);
        let mut state = lock(&self.state);
        if state.repos.contains_key(&key) {
            return ResponseTemplate::new(409);
        }
        let repo = FakeRepo {
            private: body["private"].as_bool().unwrap_or(false),
            description: body["description"].as_str().unwrap_or_default().to_string(),
            ..FakeRepo::default()
        };
        let git_dir = self.git_dir(owner, &name);
        init_forgejo_git_dir(&git_dir);
        let response = ResponseTemplate::new(201).set_body_json(repo.to_json(&git_dir));
        state.repos.insert(key, repo);
        response
    }

    fn edit(&self, owner: &str, name: &str, body: &Value) -> ResponseTemplate {
        let key = repo_key(owner, name);
        let mut state = lock(&self.state);
        if let Some(status) = state.edit_status {
            return ResponseTemplate::new(status);
        }
        let git_dir = self.git_dir(owner, name);
        let Some(repo) = state.repos.get_mut(&key) else {
            return ResponseTemplate::new(404);
        };
        if let Some(branch) = body["default_branch"].as_str() {
            repo.default_branch = branch.to_string();
            git(
                &git_dir,
                &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
            );
        }
        if let Some(description) = body["description"].as_str() {
            repo.description = description.to_string();
        }
        if let Some(has_actions) = body["has_actions"].as_bool() {
            repo.has_actions = has_actions;
        }
        ResponseTemplate::new(200).set_body_json(repo.to_json(&git_dir))
    }
}

impl Respond for ForgejoApi {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let method = request.method.as_str().to_string();
        let path = request.url.path().to_string();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let mut event = format!("forgejo:{method} {path}");
        if method == "PATCH" {
            event.push_str(&format!(" {body}"));
        }
        self.events.push(event);

        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (method.as_str(), segments.as_slice()) {
            ("GET", ["api", "v1", "user"]) => match lock(&self.state).whoami_status {
                Some(status) => ResponseTemplate::new(status),
                None => ResponseTemplate::new(200).set_body_json(json!({"login": FORGEJO_LOGIN})),
            },
            ("GET", ["api", "v1", "repos", owner, name]) => {
                let key = repo_key(owner, name);
                match lock(&self.state).repos.get(&key) {
                    Some(repo) => ResponseTemplate::new(200)
                        .set_body_json(repo.to_json(&self.git_dir(owner, name))),
                    None => ResponseTemplate::new(404),
                }
            }
            ("POST", ["api", "v1", "user", "repos"]) => self.create(FORGEJO_LOGIN, &body),
            ("POST", ["api", "v1", "orgs", owner, "repos"]) => self.create(owner, &body),
            ("PATCH", ["api", "v1", "repos", owner, name]) => self.edit(owner, name, &body),
            ("GET", ["api", "v1", "repos", owner, name, "topics"]) => {
                let key = repo_key(owner, name);
                match lock(&self.state).repos.get(&key) {
                    Some(repo) => {
                        ResponseTemplate::new(200).set_body_json(json!({"topics": repo.topics}))
                    }
                    None => ResponseTemplate::new(404),
                }
            }
            ("PUT", ["api", "v1", "repos", owner, name, "topics", topic]) => {
                let key = repo_key(owner, name);
                match lock(&self.state).repos.get_mut(&key) {
                    Some(repo) => {
                        if !repo.topics.iter().any(|existing| existing == topic) {
                            repo.topics.push((*topic).to_string());
                        }
                        ResponseTemplate::new(204)
                    }
                    None => ResponseTemplate::new(404),
                }
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

pub struct GithubApi {
    pub state: Arc<Mutex<ForgeState>>,
    pub events: Events,
}

impl Respond for GithubApi {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        self.events
            .push(format!("github:{} {path}", request.method.as_str()));
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let ["repos", owner, name] = segments.as_slice() else {
            return ResponseTemplate::new(404);
        };
        let key = repo_key(owner, name);
        match lock(&self.state).github.get(&key) {
            Some(GithubAnswer::Description(description)) => ResponseTemplate::new(200)
                .set_body_json(json!({
                    "description": description,
                    "private": false,
                    "archived": false,
                })),
            Some(GithubAnswer::Status(status)) => ResponseTemplate::new(*status),
            None => ResponseTemplate::new(404),
        }
    }
}

pub fn forgejo_git_dir(root: &Path, owner: &str, name: &str) -> PathBuf {
    root.join("forgejo").join(owner).join(format!("{name}.git"))
}

/// Creates a bare repository that, like Forgejo, refuses to delete the
/// branch its `HEAD` points at.
pub fn init_forgejo_git_dir(git_dir: &Path) {
    init_bare(git_dir);
    let hook = git_dir.join("hooks").join("pre-receive");
    std::fs::create_dir_all(hook.parent().expect("hooks dir")).expect("create hooks dir");
    write_script(
        &hook,
        "head=$(git symbolic-ref HEAD)\n\
         zero=0000000000000000000000000000000000000000\n\
         while read -r old new ref; do\n\
         \tif [ \"$new\" = \"$zero\" ] && [ \"$ref\" = \"$head\" ]; then\n\
         \t\techo \"refusing to delete the default branch $ref\" >&2\n\
         \t\texit 1\n\
         \tfi\n\
         done",
    );
}
