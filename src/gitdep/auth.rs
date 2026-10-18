use crate::node::git::fetch::GitEnv;

pub fn git_env_from(_get: &dyn Fn(&str) -> Option<String>) -> GitEnv {
    GitEnv::default()
}

pub fn machine_git_env() -> GitEnv {
    git_env_from(&|key| std::env::var(key).ok())
}

pub fn basic_auth_header(_token: &str) -> String {
    String::new()
}
