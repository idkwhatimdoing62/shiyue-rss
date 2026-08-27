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
  comment       TEXT,
  is_favorite   INTEGER NOT NULL DEFAULT 0 CHECK (is_favorite IN (0, 1)),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_article_selections_article
  ON article_selections(article_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_article_selections_favorite
  ON article_selections(is_favorite, created_at DESC, id DESC);

INSERT INTO feeds(id,url,title,next_fetch) VALUES(1,'https://fixture.example/feed','Fixture feed',0);
INSERT INTO articles(id,feed_id,entry_id,url,title,content,fetched_at) VALUES(1,1,'fixture-entry','https://fixture.example/article','Preserved article','fixture body',100);
INSERT INTO article_selections(id,article_id,selected_text,comment,is_favorite,created_at,updated_at) VALUES(1,1,'preserved quote','preserved thought',1,100,100);
PRAGMA user_version=0;
