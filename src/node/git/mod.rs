//! Node git dependencies declared in `package.json`, cloned once per commit into
//! `<store>/node/git`, then linked into projects by `kong use`.

pub mod fetch;
pub mod spec;

#[cfg(test)]
pub(crate) mod proof_support;
