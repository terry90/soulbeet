-- Nothing reads Navidrome's ReportRealPath any more: deletion and the rating sync place files
-- from the native API's library-relative path. A user marked for it is connected.
UPDATE users SET navidrome_status = 'connected' WHERE navidrome_status = 'missing_report_real_path';
