pub mod clone;
pub mod git;
pub mod lock;
pub mod paths;
pub mod spec;

pub use clone::{clone_or_update, Action};
pub use git::{resolve_local_git_dir, Git};
pub use lock::file_lock;
pub use paths::{default_cache_dir, default_config_dir, local_db_name};
pub use spec::{parse_repo_spec, LocalRepo, RemoteRepo, RepoRef};
