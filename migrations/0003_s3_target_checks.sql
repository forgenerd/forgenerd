-- Result of the last "Test" of each storage target, shown in the UI.
ALTER TABLE s3_targets ADD COLUMN last_test_at DATETIME;
ALTER TABLE s3_targets ADD COLUMN last_test_ok BOOLEAN;
ALTER TABLE s3_targets ADD COLUMN last_test_error TEXT;
