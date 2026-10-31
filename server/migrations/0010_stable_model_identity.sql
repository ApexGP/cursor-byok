-- `model_hash` now stores an opaque, stable identity instead of a content
-- hash. Editing a model must not change the name Cursor persists in its
-- model-selection state, otherwise the user's saved selection is dropped.
--
-- Rows written by older builds keep their old content hash as the stable ID so
-- existing Cursor selections and llm_calls references remain valid. Their
-- `config_hash` is backfilled with the same value because the old ID was the
-- content hash; new rows compute both independently.
ALTER TABLE model_configs ADD COLUMN config_hash TEXT;

UPDATE model_configs SET config_hash = model_hash;

CREATE UNIQUE INDEX model_configs_config_hash ON model_configs(config_hash);
