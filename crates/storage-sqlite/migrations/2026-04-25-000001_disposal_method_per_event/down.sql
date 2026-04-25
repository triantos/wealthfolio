-- SQLite supports DROP COLUMN since 3.35 (Diesel pins ≥ 3.35).

ALTER TABLE activities DROP COLUMN disposal_method;
ALTER TABLE accounts  DROP COLUMN default_disposal_method;
