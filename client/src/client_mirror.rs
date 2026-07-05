use std::collections::{HashMap, HashSet};

use wikidesk_shared::{WikiInfo, derived_wiki_path, is_valid_wiki_name, validate_local_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientMirror {
    pub(crate) name: String,
    pub(crate) local_path: String,
    pub(crate) description: Option<String>,
}

impl ClientMirror {
    pub(crate) fn new(name: String, local_path: String) -> Self {
        Self {
            name,
            local_path,
            description: None,
        }
    }

    pub(crate) fn with_description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }

    pub(crate) fn description(&self) -> anyhow::Result<&str> {
        self.description
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("wiki '{}' has no description", self.name))
    }

    pub(crate) fn spec(&self) -> String {
        if self.local_path == derived_wiki_path(&self.name) {
            self.name.clone()
        } else {
            format!("{}:{}", self.name, self.local_path)
        }
    }
}

pub(crate) fn parse_wikis(raw: &str) -> anyhow::Result<Vec<ClientMirror>> {
    let wikis = parse_wiki_specs([raw])?;
    if wikis.is_empty() {
        anyhow::bail!("WIKIDESK_WIKIS must name at least one wiki");
    }
    Ok(wikis)
}

pub(crate) fn parse_wiki_specs<I, S>(specs: I) -> anyhow::Result<Vec<ClientMirror>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen_names = HashSet::new();
    let mut seen_paths = HashSet::new();
    let mut wikis = Vec::new();
    for raw in specs {
        for spec in raw
            .as_ref()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let wiki = parse_wiki_spec(spec)?;
            if !seen_names.insert(wiki.name.clone()) {
                anyhow::bail!("duplicate wiki name '{}'", wiki.name);
            }
            if !seen_paths.insert(wiki.local_path.clone()) {
                anyhow::bail!("duplicate local_path '{}'", wiki.local_path);
            }
            wikis.push(wiki);
        }
    }
    Ok(wikis)
}

pub(crate) fn select_wikis(
    available: Vec<WikiInfo>,
    requested: Vec<String>,
) -> anyhow::Result<Vec<ClientMirror>> {
    let requested = parse_wiki_specs(requested.iter().map(String::as_str))?;
    let mut by_name = HashMap::new();
    let mut ordered = Vec::new();
    for wiki in available {
        if wiki.description.trim().is_empty() {
            anyhow::bail!("server returned empty description for wiki '{}'", wiki.name);
        }
        let mirror = ClientMirror::new(wiki.name.clone(), derived_wiki_path(&wiki.name))
            .with_description(wiki.description.clone());
        by_name.insert(wiki.name, wiki.description);
        ordered.push(mirror);
    }

    if requested.is_empty() {
        return Ok(ordered);
    }

    let mut selected = Vec::with_capacity(requested.len());
    let mut missing = Vec::new();
    for configured in requested {
        match by_name.get(&configured.name) {
            Some(description) => {
                selected.push(configured.with_description(description.clone()));
            }
            None => missing.push(configured.name),
        }
    }
    if !missing.is_empty() {
        let mut available = by_name.into_keys().collect::<Vec<_>>();
        available.sort();
        anyhow::bail!(
            "server does not advertise wiki(s): {} (available: {})",
            missing.join(", "),
            available.join(", ")
        );
    }
    Ok(selected)
}

fn parse_wiki_spec(spec: &str) -> anyhow::Result<ClientMirror> {
    let (name, local_path) = match spec.split_once(':') {
        Some((name, local_path)) => (name.trim(), Some(local_path.trim())),
        None => (spec.trim(), None),
    };
    if !is_valid_wiki_name(name) {
        anyhow::bail!(
            "invalid wiki name '{name}' (use lowercase letters, digits, and hyphens; start and end with a letter or digit)"
        );
    }
    let local_path = match local_path {
        Some(path) => {
            validate_local_path(path)
                .map_err(|e| anyhow::anyhow!("invalid local_path for wiki '{name}': {e}"))?;
            path.to_string()
        }
        None => derived_wiki_path(name),
    };
    Ok(ClientMirror::new(name.to_string(), local_path))
}
