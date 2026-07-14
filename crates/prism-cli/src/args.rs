//! A small flag parser.
//!
//! Hand-rolled on purpose. The CLI has ten subcommands and no ambition to have
//! forty; an argument-parsing dependency would be the largest thing in the
//! dependency tree, for this. See docs/DECISIONS.md, D-002.

use prism_types::error::{PrismError, Result};
use std::collections::BTreeMap;

pub struct Args {
    pub command: String,
    pub sub: Option<String>,
    flags: BTreeMap<String, String>,
    switches: Vec<String>,
}

impl Args {
    pub fn parse(argv: Vec<String>) -> Result<Args> {
        let mut it = argv.into_iter().skip(1);
        let command = it
            .next()
            .ok_or_else(|| PrismError::Invalid("no command given".into()))?;

        let rest: Vec<String> = it.collect();
        let mut sub = None;
        let mut i = 0usize;

        // A bare word right after the command is a subcommand (`golden build`).
        if let Some(first) = rest.first() {
            if !first.starts_with("--") {
                sub = Some(first.clone());
                i = 1;
            }
        }

        let mut flags = BTreeMap::new();
        let mut switches = Vec::new();

        while i < rest.len() {
            let tok = &rest[i];
            if let Some(name) = tok.strip_prefix("--") {
                if let Some((k, v)) = name.split_once('=') {
                    flags.insert(k.to_string(), v.to_string());
                    i += 1;
                } else if i + 1 < rest.len() && !rest[i + 1].starts_with("--") {
                    flags.insert(name.to_string(), rest[i + 1].clone());
                    i += 2;
                } else {
                    switches.push(name.to_string());
                    i += 1;
                }
            } else {
                return Err(PrismError::Invalid(format!("unexpected argument `{tok}`")));
            }
        }

        Ok(Args {
            command,
            sub,
            flags,
            switches,
        })
    }

    pub fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    pub fn opt(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(|s| s.as_str())
    }

    pub fn req(&self, name: &str) -> Result<&str> {
        self.opt(name)
            .ok_or_else(|| PrismError::Invalid(format!("missing required flag --{name}")))
    }

    pub fn parse_opt<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T> {
        match self.opt(name) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|_| PrismError::Invalid(format!("--{name} `{v}` is not valid"))),
        }
    }

    pub fn parse_some<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>> {
        match self.opt(name) {
            None => Ok(None),
            Some(v) => v
                .parse()
                .map(Some)
                .map_err(|_| PrismError::Invalid(format!("--{name} `{v}` is not valid"))),
        }
    }
}
