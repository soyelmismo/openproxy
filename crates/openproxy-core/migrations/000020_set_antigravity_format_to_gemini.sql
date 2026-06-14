-- Set antigravity providers to use Gemini format
UPDATE providers SET format = 'gemini' WHERE id IN ('antigravity', 'antigravity-cli');
