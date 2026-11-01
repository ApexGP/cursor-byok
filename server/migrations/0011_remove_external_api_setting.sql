-- The external `/byok/v1` API was removed. Drop its persisted row so old API
-- keys do not linger in the database after the feature is gone.
DELETE FROM service_settings WHERE setting_key = 'external_api';
