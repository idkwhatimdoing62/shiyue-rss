CREATE TABLE IF NOT EXISTS feeds (
  id            INTEGER PRIMARY KEY,
  url           TEXT NOT NULL UNIQUE,
  title         TEXT,
  interval_secs INTEGER,
  last_fetch    INTEGER,
  next_fetch    INTEGER NOT NULL DEFAULT 0,
  last_error    TEXT,
  fail_count    INTEGER NOT NULL DEFAULT 0,
  disabled      INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS articles (
  id         INTEGER PRIMARY KEY,
  feed_id    INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
  entry_id   TEXT NOT NULL,
  url        TEXT,
  title      TEXT,
  author     TEXT,
  published  INTEGER,
  content    TEXT,
  is_read    INTEGER NOT NULL DEFAULT 0,
  starred    INTEGER NOT NULL DEFAULT 0,
  read_later INTEGER NOT NULL DEFAULT 0,
  archived   INTEGER NOT NULL DEFAULT 0,
  fetched_at INTEGER NOT NULL,
  UNIQUE(feed_id, entry_id)
);
CREATE TABLE IF NOT EXISTS article_selections (
  id            INTEGER PRIMARY KEY,
  article_id    INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
  selected_text TEXT NOT NULL CHECK (length(trim(selected_text)) > 0),
  start_offset  INTEGER,
  end_offset    INTEGER,
  anchor_prefix TEXT NOT NULL DEFAULT '',
  anchor_suffix TEXT NOT NULL DEFAULT '',
  comment       TEXT,
  is_favorite   INTEGER NOT NULL DEFAULT 0 CHECK (is_favorite IN (0, 1)),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_article_selections_article
  ON article_selections(article_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_article_selections_favorite
  ON article_selections(is_favorite, created_at DESC, id DESC);
CREATE TABLE IF NOT EXISTS tags (
  id         INTEGER PRIMARY KEY,
  name       TEXT NOT NULL COLLATE NOCASE UNIQUE,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS article_tags (
  article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
  tag_id     INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(article_id, tag_id)
);
CREATE INDEX IF NOT EXISTS idx_article_tags_tag ON article_tags(tag_id, article_id);
CREATE TABLE IF NOT EXISTS search_history (
  query         TEXT PRIMARY KEY COLLATE NOCASE,
  last_used_at  INTEGER NOT NULL,
  use_count     INTEGER NOT NULL DEFAULT 1,
  result_count  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS resources (
  id                 INTEGER PRIMARY KEY,
  url                TEXT NOT NULL,
  canonical_url      TEXT NOT NULL UNIQUE,
  parent_resource_id INTEGER REFERENCES resources(id) ON DELETE SET NULL,
  linked_article_id  INTEGER REFERENCES articles(id) ON DELETE SET NULL,
  kind               TEXT NOT NULL CHECK (kind IN ('site', 'page', 'article')),
  title              TEXT,
  purpose_zh         TEXT,
  purpose_source     TEXT CHECK (purpose_source IS NULL OR purpose_source IN ('manual','ai')),
  use_when_zh        TEXT,
  use_when_source    TEXT CHECK (use_when_source IS NULL OR use_when_source IN ('manual','ai')),
  capabilities       TEXT NOT NULL DEFAULT '[]',
  limitations        TEXT NOT NULL DEFAULT '[]',
  pricing            TEXT CHECK (pricing IS NULL OR pricing IN ('free', 'freemium', 'paid', 'unknown')),
  requires_login     INTEGER CHECK (requires_login IS NULL OR requires_login IN (0, 1)),
  languages          TEXT NOT NULL DEFAULT '[]',
  private_note       TEXT,
  privacy            TEXT NOT NULL DEFAULT 'public' CHECK (privacy IN ('public', 'private')),
  status             TEXT NOT NULL CHECK (status IN ('pending_review', 'enrichment_pending', 'active', 'broken', 'archived')),
  source             TEXT NOT NULL CHECK (source IN ('gui', 'cli_agent', 'import')),
  manual_rating      INTEGER CHECK (manual_rating IS NULL OR manual_rating BETWEEN 1 AND 5),
  latest_snapshot_id INTEGER REFERENCES resource_snapshots(id) ON DELETE SET NULL,
  last_checked_at    INTEGER,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_resources_status_updated ON resources(status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_resources_parent ON resources(parent_resource_id);
CREATE INDEX IF NOT EXISTS idx_resources_article ON resources(linked_article_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_resources_import_article
  ON resources(linked_article_id) WHERE source='import' AND linked_article_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS resource_categories (
  resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  category    TEXT NOT NULL CHECK (category IN ('tool', 'asset-library', 'docs', 'blog', 'inspiration', 'service', 'repository', 'other')),
  PRIMARY KEY(resource_id, category)
);
CREATE TABLE IF NOT EXISTS resource_tags (
  resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  name        TEXT NOT NULL COLLATE NOCASE,
  language    TEXT NOT NULL CHECK (language IN ('zh', 'en', 'other')),
  source      TEXT NOT NULL CHECK (source IN ('manual', 'ai')),
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(resource_id, name)
);
CREATE TABLE IF NOT EXISTS resource_snapshots (
  id              INTEGER PRIMARY KEY,
  resource_id     INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  content_hash    TEXT,
  fetched_url     TEXT,
  http_status     INTEGER,
  title           TEXT,
  cleaned_content TEXT,
  fetched_at      INTEGER NOT NULL,
  fetch_error     TEXT,
  CHECK (content_hash IS NOT NULL OR fetch_error IS NOT NULL)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_resource_snapshot_success
  ON resource_snapshots(resource_id, content_hash) WHERE content_hash IS NOT NULL;
CREATE TABLE IF NOT EXISTS resource_enrichment_runs (
  id            INTEGER PRIMARY KEY,
  resource_id   INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  snapshot_id   INTEGER REFERENCES resource_snapshots(id) ON DELETE SET NULL,
  provider      TEXT NOT NULL,
  model         TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  schema_version TEXT NOT NULL,
  started_at    INTEGER NOT NULL,
  finished_at   INTEGER,
  status        TEXT NOT NULL CHECK (status IN ('pending', 'running', 'succeeded', 'failed')),
  error_code    TEXT,
  error_message TEXT
);
CREATE TABLE IF NOT EXISTS resource_usage_events (
  id          INTEGER PRIMARY KEY,
  resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
  event       TEXT NOT NULL CHECK (event IN ('returned', 'confirmed_used')),
  occurred_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_resource_usage_events ON resource_usage_events(resource_id, occurred_at DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS resource_fts USING fts5(
  source_id UNINDEXED,
  body,
  tokenize='trigram'
);
CREATE VIRTUAL TABLE IF NOT EXISTS library_fts USING fts5(
  kind UNINDEXED,
  source_id UNINDEXED,
  article_id UNINDEXED,
  body,
  tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS articles_fts_insert AFTER INSERT ON articles BEGIN
  INSERT INTO library_fts(kind, source_id, article_id, body)
  VALUES (
    CASE WHEN (SELECT url FROM feeds WHERE id = new.feed_id) = 'shiyue://web-clippings' THEN 1 ELSE 0 END,
    new.id,
    new.id,
    trim(COALESCE(new.title, '') || char(10) || COALESCE(new.author, '') || char(10) ||
         COALESCE(new.content, '') || char(10) || COALESCE(new.url, ''))
  );
END;
CREATE TRIGGER IF NOT EXISTS articles_fts_update AFTER UPDATE OF title, author, content, url, feed_id ON articles BEGIN
  DELETE FROM library_fts WHERE kind IN (0, 1) AND source_id = old.id;
  INSERT INTO library_fts(kind, source_id, article_id, body)
  VALUES (
    CASE WHEN (SELECT url FROM feeds WHERE id = new.feed_id) = 'shiyue://web-clippings' THEN 1 ELSE 0 END,
    new.id,
    new.id,
    trim(COALESCE(new.title, '') || char(10) || COALESCE(new.author, '') || char(10) ||
         COALESCE(new.content, '') || char(10) || COALESCE(new.url, '') || char(10) ||
         COALESCE((SELECT group_concat(t.name, ' ') FROM article_tags at
                   JOIN tags t ON t.id = at.tag_id WHERE at.article_id = new.id), ''))
  );
END;
CREATE TRIGGER IF NOT EXISTS articles_fts_delete AFTER DELETE ON articles BEGIN
  DELETE FROM library_fts WHERE article_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS selections_fts_insert AFTER INSERT ON article_selections BEGIN
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT 2, new.id, new.article_id, new.selected_text WHERE new.is_favorite = 1;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT 3, new.id, new.article_id, new.comment
    WHERE new.comment IS NOT NULL AND length(trim(new.comment)) > 0;
END;
CREATE TRIGGER IF NOT EXISTS selections_fts_update AFTER UPDATE ON article_selections BEGIN
  DELETE FROM library_fts WHERE kind IN (2, 3) AND source_id = old.id;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT 2, new.id, new.article_id, new.selected_text WHERE new.is_favorite = 1;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT 3, new.id, new.article_id, new.comment
    WHERE new.comment IS NOT NULL AND length(trim(new.comment)) > 0;
END;
CREATE TRIGGER IF NOT EXISTS selections_fts_delete AFTER DELETE ON article_selections BEGIN
  DELETE FROM library_fts WHERE kind IN (2, 3) AND source_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS article_tags_fts_insert AFTER INSERT ON article_tags BEGIN
  DELETE FROM library_fts WHERE kind IN (0, 1) AND source_id = new.article_id;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT CASE WHEN f.url = 'shiyue://web-clippings' THEN 1 ELSE 0 END, a.id, a.id,
           trim(COALESCE(a.title, '') || char(10) || COALESCE(a.author, '') || char(10) ||
                COALESCE(a.content, '') || char(10) || COALESCE(a.url, '') || char(10) ||
                COALESCE((SELECT group_concat(t.name, ' ') FROM article_tags at
                          JOIN tags t ON t.id = at.tag_id WHERE at.article_id = a.id), ''))
    FROM articles a JOIN feeds f ON f.id = a.feed_id WHERE a.id = new.article_id;
END;
CREATE TRIGGER IF NOT EXISTS article_tags_fts_delete AFTER DELETE ON article_tags BEGIN
  DELETE FROM library_fts WHERE kind IN (0, 1) AND source_id = old.article_id;
  INSERT INTO library_fts(kind, source_id, article_id, body)
    SELECT CASE WHEN f.url = 'shiyue://web-clippings' THEN 1 ELSE 0 END, a.id, a.id,
           trim(COALESCE(a.title, '') || char(10) || COALESCE(a.author, '') || char(10) ||
                COALESCE(a.content, '') || char(10) || COALESCE(a.url, '') || char(10) ||
                COALESCE((SELECT group_concat(t.name, ' ') FROM article_tags at
                          JOIN tags t ON t.id = at.tag_id WHERE at.article_id = a.id), ''))
    FROM articles a JOIN feeds f ON f.id = a.feed_id WHERE a.id = old.article_id;
END;

INSERT INTO feeds(id,url,title,next_fetch) VALUES(1,'https://fixture.example/feed','Fixture feed',0);
INSERT INTO articles(id,feed_id,entry_id,url,title,content,fetched_at) VALUES(1,1,'fixture-entry','https://fixture.example/article','Preserved article','fixture body',100);
INSERT INTO article_selections(id,article_id,selected_text,comment,is_favorite,created_at,updated_at) VALUES(1,1,'preserved quote','preserved thought',1,100,100);
INSERT INTO resources(id,url,canonical_url,kind,title,capabilities,limitations,languages,privacy,status,source,created_at,updated_at) VALUES(1,'https://fixture.example/tool','https://fixture.example/tool','site','Preserved resource','[]','[]','[]','public','active','gui',100,100);
PRAGMA user_version=0;
