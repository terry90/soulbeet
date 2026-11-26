package org.soulbeet.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.soulbeet.app.data.LyricLine
import org.soulbeet.app.data.LyricsSync

class LyricsSyncTest {

    @Test
    fun parseLrcHandlesMetadataCentisecondsMillisecondsAndRepeatedTags() {
        val raw = """
            [ar:Daft Punk]
            [ti:Instant Crush]
            [00:12.50]I didn't want to be the one to forget
            [00:25.125][01:05.00]And we will never be alone again
            [00:18.0]I thought of everything I'd never regret
        """.trimIndent()

        val parsed = LyricsSync.parseLrc(raw)
        assertEquals(4, parsed.size)

        // Must be sorted chronologically
        assertEquals(12_500L, parsed[0].startMs)
        assertEquals("I didn't want to be the one to forget", parsed[0].text)

        assertEquals(18_000L, parsed[1].startMs)
        assertEquals("I thought of everything I'd never regret", parsed[1].text)

        assertEquals(25_125L, parsed[2].startMs)
        assertEquals("And we will never be alone again", parsed[2].text)

        assertEquals(65_000L, parsed[3].startMs)
        assertEquals("And we will never be alone again", parsed[3].text)
    }

    @Test
    fun parseLrcFallsBackToUnsyncedLinesWhenNoTimestampsPresent() {
        val raw = """
            First unsynced line
            Second unsynced line
        """.trimIndent()

        val parsed = LyricsSync.parseLrc(raw)
        assertEquals(2, parsed.size)
        assertNull(parsed[0].startMs)
        assertEquals("First unsynced line", parsed[0].text)
        assertEquals("Second unsynced line", parsed[1].text)
    }

    @Test
    fun findActiveLyricIndexBinarySearchHandlesBoundariesAndOffset() {
        val lines = listOf(
            LyricLine(startMs = 5_000L, text = "Line 0"),
            LyricLine(startMs = 12_000L, text = "Line 1"),
            LyricLine(startMs = 20_000L, text = "Line 2"),
            LyricLine(startMs = 35_000L, text = "Line 3"),
        )

        // Before first line
        assertEquals(-1, LyricsSync.findActiveLyricIndex(lines, positionMs = 0L))
        assertEquals(-1, LyricsSync.findActiveLyricIndex(lines, positionMs = 4_999L))

        // Exact timestamps
        assertEquals(0, LyricsSync.findActiveLyricIndex(lines, positionMs = 5_000L))
        assertEquals(1, LyricsSync.findActiveLyricIndex(lines, positionMs = 12_000L))
        assertEquals(2, LyricsSync.findActiveLyricIndex(lines, positionMs = 25_000L))
        assertEquals(3, LyricsSync.findActiveLyricIndex(lines, positionMs = 99_000L))

        // Positive offset shifts lookup forward by 500ms
        assertEquals(1, LyricsSync.findActiveLyricIndex(lines, positionMs = 11_600L, offsetMs = 500L))
        // Negative offset shifts lookup backward by 500ms
        assertEquals(0, LyricsSync.findActiveLyricIndex(lines, positionMs = 12_200L, offsetMs = -500L))
    }

    @Test
    fun findActiveLyricIndexReturnsNegativeOneForUnsyncedList() {
        val unsynced = listOf(
            LyricLine(startMs = null, text = "Hello"),
            LyricLine(startMs = null, text = "World"),
        )
        assertEquals(-1, LyricsSync.findActiveLyricIndex(unsynced, positionMs = 15_000L))
        assertEquals(-1, LyricsSync.findActiveLyricIndex(emptyList(), positionMs = 15_000L))
    }
}
