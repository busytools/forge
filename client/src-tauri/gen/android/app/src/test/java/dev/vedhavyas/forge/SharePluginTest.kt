package dev.vedhavyas.forge

import android.content.Intent
import java.io.ByteArrayInputStream
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The share intake's own rules, pinned off the device.
 *
 * **What counts as a share, and what a copied file is called**, are the two
 * places a wrong answer is silent on a phone: a share that reads as none
 * opens an empty sheet, and a name that keeps a separator writes outside the
 * cache directory. Both are pure functions for that reason.
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
  fun aShareWithNoStreamHasNoToken() {
    assertNull(SharePlugin.shareToken(Intent.ACTION_SEND, "image/png", emptyList()))
  }

  @Test
  fun oneShareKeepsOneTokenAndAnotherShareDoesNot() {
    val first = SharePlugin.shareToken(Intent.ACTION_SEND, "image/png", listOf("content://a"))
    assertEquals(
      first,
      SharePlugin.shareToken(Intent.ACTION_SEND, "image/png", listOf("content://a")),
    )
    assertNotEquals(
      first,
      SharePlugin.shareToken(Intent.ACTION_SEND, "image/png", listOf("content://b")),
    )
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
  fun aLongDisplayNameIsCapped() {
    val name = SharePlugin.cacheName("a".repeat(400))
    assertEquals(120, name.length)
  }

  @Test
  fun aCopyReportsTheBytesThatLandedAndNotTheProvidersClaim() {
    val dir = File.createTempFile("share", "test").let { file ->
      file.delete()
      file.mkdirs()
      file
    }
    try {
      val copied =
        SharePlugin.copyInto(dir, "note.txt", "text/plain") {
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
  fun twoFilesOfOneNameCopyToTwoPaths() {
    val dir = File.createTempFile("share", "test").let { file ->
      file.delete()
      file.mkdirs()
      file
    }
    try {
      val first =
        SharePlugin.copyInto(dir, "photo.jpg", "image/jpeg") {
          ByteArrayInputStream("one".toByteArray())
        }
      val second =
        SharePlugin.copyInto(dir, "photo.jpg", "image/jpeg") {
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
}
