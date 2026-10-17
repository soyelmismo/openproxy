-- Add selected_accounts JSON array to combo_targets for multi-account checklist selection
ALTER TABLE combo_targets ADD COLUMN selected_accounts TEXT DEFAULT NULL;
