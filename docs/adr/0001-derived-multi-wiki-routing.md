# Derive wiki routing and storage from wiki names

wikidesk always serves one or more named wiki instances rather than a special single-wiki mode. Each wiki name is a strict slug that derives the server wiki repo (`wiki-{name}`), client mirror path (`wiki-{name}`), and HTTP base path (`/{name}`), trading flexible per-wiki paths for predictable client setup, safer routing, and less configuration drift.
