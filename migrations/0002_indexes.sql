CREATE UNIQUE INDEX uq_file_root_name
  ON file_entry(project_id, exact_name COLLATE WINDOWS_ORDINAL_CI_V1)
  WHERE parent_id IS NULL AND state IN ('live', 'settling');

CREATE UNIQUE INDEX uq_file_child_name
  ON file_entry(project_id, parent_id, exact_name COLLATE WINDOWS_ORDINAL_CI_V1)
  WHERE parent_id IS NOT NULL AND state IN ('live', 'settling');

CREATE INDEX ix_file_platform_identity
  ON file_entry(project_id, platform_kind, volume_serial, filesystem_file_id);

CREATE UNIQUE INDEX uq_upload_root_destination
  ON upload_session(
    project_id,
    destination_name COLLATE WINDOWS_ORDINAL_CI_V1
  )
  WHERE destination_parent_id IS NULL
    AND state IN ('created', 'uploading', 'verifying', 'committing');

CREATE UNIQUE INDEX uq_upload_child_destination
  ON upload_session(
    project_id,
    destination_parent_id,
    destination_name COLLATE WINDOWS_ORDINAL_CI_V1
  )
  WHERE destination_parent_id IS NOT NULL
    AND state IN ('created', 'uploading', 'verifying', 'committing');
