package org.soulbeet.app.data

/**
 * Single lyric line; [startMs] is non-null for synced karaoke lyrics.
 */
data class LyricLine(
    val startMs: Long?,
    val text: String,
)

/**
 * Parsed lyrics payload for the active track.
 */
data class TrackLyricsData(
    val displayArtist: String? = null,
    val displayTitle: String? = null,
    val lang: String = "",
    val synced: Boolean = false,
    val offsetMs: Long = 0L,
    val lines: List<LyricLine> = emptyList(),
)

/**
 * Pure LRC timestamp parser and binary-search active-line resolver for Synced Karaoke Lyrics.
 */
object LyricsSync {

    private val TIMESTAMP_REGEX = Regex("""\[(\d{1,3}):(\d{2})(?:[.:](\d{1,3}))?]""")
    private val META_TAG_REGEX = Regex("""^\[[a-zA-Z]{2,6}:.*]$""")

    /**
     * Parses standard LRC text (supporting multiple `[mm:ss.xx]` timestamps per line, centiseconds
     * or milliseconds, and unsynced fallback lines) into a chronologically sorted [List] of
     * [LyricLine].
     */
    fun parseLrc(raw: String): List<LyricLine> {
        if (raw.isBlank()) return emptyList()

        val timed = mutableListOf<LyricLine>()
        val untimed = mutableListOf<LyricLine>()

        for (rawLine in raw.lineSequence()) {
            val trimmed = rawLine.trim()
            if (trimmed.isEmpty()) continue

            val matches = TIMESTAMP_REGEX.findAll(trimmed).toList()
            if (matches.isEmpty()) {
                if (!META_TAG_REGEX.matches(trimmed)) {
                    untimed.add(LyricLine(startMs = null, text = trimmed))
                }
                continue
            }

            val lyricText = TIMESTAMP_REGEX.replace(trimmed, "").trim()
            if (lyricText.isEmpty()) continue

            for (match in matches) {
                val mins = match.groupValues[1].toLongOrNull() ?: continue
                val secs = match.groupValues[2].toLongOrNull() ?: continue
                val fracRaw = match.groupValues[3]
                val fracMs = when (fracRaw.length) {
                    0 -> 0L
                    1 -> (fracRaw.toLongOrNull() ?: 0L) * 100L
                    2 -> (fracRaw.toLongOrNull() ?: 0L) * 10L
                    else -> fracRaw.take(3).toLongOrNull() ?: 0L
                }
                val startMs = (mins * 60_000L) + (secs * 1_000L) + fracMs
                timed.add(LyricLine(startMs = startMs, text = lyricText))
            }
        }

        return if (timed.isNotEmpty()) {
            timed.sortedBy { it.startMs ?: Long.MAX_VALUE }
        } else {
            untimed
        }
    }

    /**
     * Finds the index of the active synced lyric line for [positionMs] (applying [offsetMs]) via
     * binary search. Returns `-1` if no synced line has started yet or the list has no timestamps.
     */
    fun findActiveLyricIndex(
        lines: List<LyricLine>,
        positionMs: Long,
        offsetMs: Long = 0L,
    ): Int {
        if (lines.isEmpty()) return -1
        val effectivePos = positionMs + offsetMs

        // Fast-check if list is synced (contains at least one timestamp)
        val syncedIndices = ArrayList<Int>(lines.size)
        for (i in lines.indices) {
            if (lines[i].startMs != null) {
                syncedIndices.add(i)
            }
        }
        if (syncedIndices.isEmpty()) return -1

        var lo = 0
        var hi = syncedIndices.lastIndex
        var best = -1
        while (lo <= hi) {
            val mid = (lo + hi) ushr 1
            val lineIdx = syncedIndices[mid]
            val ts = lines[lineIdx].startMs ?: Long.MAX_VALUE
            if (ts <= effectivePos) {
                best = lineIdx
                lo = mid + 1
            } else {
                hi = mid - 1
            }
        }
        return best
    }
}
