package dev.vedhavyas.forge

import android.content.Intent
import java.io.ByteArrayInputStream
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * The share intake's own rules, pinned off the device.
 *
 * **What counts as a share, what a copied file is called and whether a reload
 * re-answers a share** are the places a wrong answer is silent on a phone: a
 * share that reads as none opens an empty sheet, a name that keeps a
 * separator writes outside the cache directory, and a reload that re-answers
 * prompts a share twice. They are pure functions for that reason.
 */
class SharePluginTest {
  @Test
  fun aSingleShareTakesItsOneStream() {
    assertEquals(
      listOf("content://media/1"),
      SharePlugin.streamStrings(Intent.ACTION_SEND, "content://media/1", null),
    )
  }

  @Test
  fun aMultipleShareTakesEveryStreamInOrder() {
    assertEquals(
      listOf("content://media/1", "content://media/2"),
      SharePlugin.streamStrings(
        Intent.ACTION_SEND_MULTIPLE,
        null,
        listOf("content://media/1", "content://media/2"),
      ),
    )
  }

  @Test
  fun anIntentThatIsNotAShareTakesNothing() {
    assertEquals(emptyList<String>(), SharePlugin.streamStrings(Intent.ACTION_MAIN, "content://a", null))
    assertEquals(emptyList<String>(), SharePlugin.streamStrings(null, null, null))
  }

  @Test
  fun aShareWithNoStreamStillHasAToken() {
    // **A SEND that carried no stream is still a share this app takes**: the
    // token is what lets the sheet say it carried no file instead of opening
    // on nothing. Only an intent that is not a share has none.
    assertEquals(
      listOf(PROCESS.toString(), LAUNCH.toString(), Intent.ACTION_SEND, "image/png", "")
        .joinToString("\u0000"),
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_SEND, "image/png", emptyList()),
    )
    assertNull(
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_MAIN, "image/png", listOf("content://a"))
    )
    assertNull(SharePlugin.shareToken(LAUNCH, PROCESS, null, null, emptyList()))
  }

  @Test
  fun oneShareKeepsOneTokenAndAnotherShareDoesNot() {
    val first =
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_SEND, "image/png", listOf("content://a"))
    assertEquals(
      first,
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_SEND, "image/png", listOf("content://a")),
    )
    assertNotEquals(
      first,
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_SEND, "image/png", listOf("content://b")),
    )
  }

  @Test
  fun aTokenAlreadyAnsweredIsNotAnsweredAgain() {
    // **The guard the reload test cannot see**: a token equal to the answered
    // one resolves empty, so a page reload does not copy and open the sheet a
    // second time. Mutating this to `token != null` reddens the test.
    assertTrue(SharePlugin.shouldAnswer("a", null))
    assertTrue(SharePlugin.shouldAnswer("b", "a"))
    assertTrue(!SharePlugin.shouldAnswer("a", "a"))
    assertTrue(!SharePlugin.shouldAnswer(null, null))
    assertTrue(!SharePlugin.shouldAnswer(null, "a"))
  }

  /// **A page reload inside one launch is not a second share**: the launch
  /// and process stamps are unchanged, so the token matches the answered one.
  @Test
  fun aReloadInsideOneLaunchIsNotASecondShare() {
    val token =
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_SEND, "image/png", listOf("content://a"))
    assertTrue(!SharePlugin.shouldAnswer(token, token))
  }

  /// **A new launch with the same files is a new share**, even in one
  /// process: the activity was created again, so the launch number moved.
  @Test
  fun aNewLaunchWithTheSameFilesIsAnsweredAgain() {
    val first =
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_SEND, "image/png", listOf("content://a"))
    val second =
      SharePlugin.shareToken(
        LAUNCH + 1,
        PROCESS,
        Intent.ACTION_SEND,
        "image/png",
        listOf("content://a"),
      )
    assertNotEquals(first, second)
    assertTrue(SharePlugin.shouldAnswer(second, first))
  }

  /// **A re-share after the process ended is answered again.** The launch
  /// counter starts over at one in the new process, so content and launch are
  /// identical to the share already answered; the process stamp is what tells
  /// them apart.
  @Test
  fun aReShareAfterAProcessRestartIsAnsweredAgain() {
    val before =
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_SEND, "image/png", listOf("content://a"))
    val after =
      SharePlugin.shareToken(
        LAUNCH,
        PROCESS + 1,
        Intent.ACTION_SEND,
        "image/png",
        listOf("content://a"),
      )
    assertNotEquals(before, after)
    assertTrue(SharePlugin.shouldAnswer(after, before))
  }

  /// **A restored activity re-delivers the intent it already answered.** Its
  /// stamps cannot match the previous process's, so the content is what says
  /// it is the same delivery - and a restored ask for different files is
  /// still a share to answer.
  @Test
  fun aRestoredActivityDoesNotAnswerTheSameLaunchIntentAgain() {
    val before =
      SharePlugin.shareToken(LAUNCH, PROCESS, Intent.ACTION_SEND, "image/png", listOf("content://a"))
    val restored =
      SharePlugin.shareToken(
        LAUNCH,
        PROCESS + 1,
        Intent.ACTION_SEND,
        "image/png",
        listOf("content://a"),
      )
    assertTrue(!SharePlugin.shouldAnswer(restored, before, restored = true))
    val other =
      SharePlugin.shareToken(
        LAUNCH,
        PROCESS + 1,
        Intent.ACTION_SEND,
        "image/png",
        listOf("content://b"),
      )
    assertTrue(SharePlugin.shouldAnswer(other, before, restored = true))
  }

  @Test
  fun aDisplayNameIsNeverAPath() {
    assertEquals("photo.jpg", SharePlugin.cacheName("../../photo.jpg"))
    assertEquals("photo.jpg", SharePlugin.cacheName("DCIM/Camera/photo.jpg"))
    assertEquals("photo.jpg", SharePlugin.cacheName("DCIM\\Camera\\photo.jpg"))
    assertEquals("file", SharePlugin.cacheName(".."))
    assertEquals("file", SharePlugin.cacheName("."))
    assertEquals("file", SharePlugin.cacheName(""))
    assertEquals("file", SharePlugin.cacheName(null))
  }

  @Test
  fun aLongDisplayNameIsCappedInBytes() {
    assertEquals(120, SharePlugin.cacheName("a".repeat(400)).length)
    // **The cap is bytes, not characters**: an astral character is four bytes
    // and two UTF-16 units, so 120 bytes holds 30 of them - 60 UTF-16 units.
    val astral = SharePlugin.cacheName("📷".repeat(200))
    assertEquals(60, astral.length)
    assertEquals(30, astral.codePointCount(0, astral.length))
    assertEquals(120, astral.toByteArray(Charsets.UTF_8).size)
  }

  @Test
  fun aCopyReportsTheBytesThatLandedAndNotTheProvidersClaim() {
    val dir = tempDir()
    try {
      val copied =
        SharePlugin.copyInto(dir, "note.txt", "text/plain", BUDGET) {
          ByteArrayInputStream("hello".toByteArray())
        }
      assertEquals("note.txt", copied.name)
      assertEquals(5L, copied.size)
      assertEquals("text/plain", copied.mime)
      assertEquals("hello", File(copied.path).readText())
    } finally {
      dir.deleteRecursively()
    }
  }

  @Test
  fun aCopyOverItsBudgetIsRefusedAndLeavesNothing() {
    // **A provider that under-reports its length cannot get past the cap**:
    // the bytes are counted as they are written, and a copy over its budget
    // is refused with the partial deleted rather than left in the cache.
    val dir = tempDir()
    try {
      try {
        SharePlugin.copyInto(dir, "big.bin", null, 4) {
          ByteArrayInputStream("hello".toByteArray())
        }
        fail("a copy over its budget must be refused")
      } catch (error: IllegalStateException) {
        assertTrue(error.message!!.contains("larger than 200 MB"))
      }
      assertEquals(0, dir.listFiles()?.size ?: 0)
    } finally {
      dir.deleteRecursively()
    }
  }

  /// **The budget is the share's, not one file's**: what one file used leaves
  /// the next less, and a share past the wall is refused rather than wrapping.
  @Test
  fun aShareBudgetCarriesAcrossItsFiles() {
    assertEquals(5L, SharePlugin.landedAfter(2L, 3L))
    try {
      SharePlugin.landedAfter(BUDGET - 1, 2)
      fail("a share past the wall must be refused")
    } catch (error: IllegalArgumentException) {
      assertTrue(error.message!!.contains("larger than 200 MB"))
    }
  }

  /// **A provider that answers zero is a stall, not progress**: the copy loop
  /// would otherwise spin for ever on a provider that never advances.
  @Test
  fun aProviderThatStopsSendingIsRefused() {
    val dir = tempDir()
    try {
      try {
        SharePlugin.copyInto(dir, "stall.bin", null, BUDGET) {
          object : ByteArrayInputStream("abc".toByteArray()) {
            var reads = 0

            override fun read(b: ByteArray, off: Int, len: Int): Int {
              reads += 1
              return if (reads == 1) super.read(b, off, len) else 0
            }
          }
        }
        fail("a stalled copy must be refused")
      } catch (error: IllegalStateException) {
        assertTrue(error.message!!.contains("stopped"))
      }
      assertEquals(0, dir.listFiles()?.size ?: 0)
    } finally {
      dir.deleteRecursively()
    }
  }

  @Test
  fun twoFilesOfOneNameCopyToTwoPaths() {
    val dir = tempDir()
    try {
      val first =
        SharePlugin.copyInto(dir, "photo.jpg", "image/jpeg", BUDGET) {
          ByteArrayInputStream("one".toByteArray())
        }
      val second =
        SharePlugin.copyInto(dir, "photo.jpg", "image/jpeg", BUDGET) {
          ByteArrayInputStream("two".toByteArray())
        }
      assertNotEquals(first.path, second.path)
      assertTrue(File(second.path).name.startsWith("photo-2"))
      assertEquals("one", File(first.path).readText())
      assertEquals("two", File(second.path).readText())
    } finally {
      dir.deleteRecursively()
    }
  }

  private fun tempDir(): File =
    File.createTempFile("share", "test").let { file ->
      file.delete()
      file.mkdirs()
      file
    }

  private companion object {
    const val BUDGET = 200L * 1024 * 1024

    /** The launch and process stamps a token carries in the tests. */
    const val LAUNCH = 1
    const val PROCESS = 7_000L
  }
}
