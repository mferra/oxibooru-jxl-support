ALTER TABLE post ADD COLUMN phash BIGINT;
CREATE INDEX idx_post_phash ON post (phash) WHERE phash IS NOT NULL;
