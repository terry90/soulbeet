# Keep MediaLibraryService, ContentProvider, and WorkManager worker classes.
-keep class org.soulbeet.media.SoulbeetMediaLibraryService { *; }
-keep class org.soulbeet.media.CoverArtContentProvider { *; }
-keep class org.soulbeet.media.OfflineSyncWorker { *; }
