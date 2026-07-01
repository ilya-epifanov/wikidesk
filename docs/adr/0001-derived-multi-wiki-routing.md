# Derive wiki routing and storage from wiki names

wikidesk always serves one or more named wiki instances rather than a special single-wiki mode. Each wiki name is a strict slug that derives the server wiki repo (`wiki-{name}`, except `default` uses `wiki`), default client mirror path (`wiki-{name}`, except `default` uses `wiki`), and HTTP base path (`/wiki/{name}`). Clients may override local mirror paths with `name:relative/path`; those paths stay client-side except when passed as `local_path` to render returned wikilinks.
