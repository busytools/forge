package dev.vedhavyas.forge

/**
 * The JNI door to the vendored libnode: [startNode] blocks on `node::Start`
 * for the life of the runtime, so it is called on a thread of its own (see
 * `BrowserEngine.startDriver`). libnode is loaded first because the shim
 * links against it.
 */
object NodeHost {
  init {
    System.loadLibrary("node")
    System.loadLibrary("nodehost")
  }

  external fun startNode(args: Array<String>, modulesPath: String): Int
}
