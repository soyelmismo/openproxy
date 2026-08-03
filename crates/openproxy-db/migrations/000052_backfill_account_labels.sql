-- Backfill empty account labels from email when available
UPDATE accounts SET label = email WHERE email IS NOT NULL AND email != '' AND (label IS NULL OR label = '');
