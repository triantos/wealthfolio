-- Per-account default disposal method, snapshotted onto each disposal
-- activity at write time so changes to the default never rewrite history.

ALTER TABLE accounts
    ADD COLUMN default_disposal_method TEXT NOT NULL DEFAULT 'FIFO'
        CHECK (default_disposal_method IN ('FIFO', 'LIFO', 'HIFO'));

ALTER TABLE activities
    ADD COLUMN disposal_method TEXT
        CHECK (disposal_method IS NULL OR disposal_method IN ('FIFO', 'LIFO', 'HIFO'));
