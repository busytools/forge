<script lang="ts">
  /**
   * The diff viewer, stood in for in tests: the shapes a live page renders,
   * which is the whole of the canvas's contract with it.
   *
   * jsdom cannot mount the real viewer - its text measurement allocates
   * without bound there - so the table below is a capture of the unified
   * shape, the same ones `diff.dom.test.ts` reads the page's decisions off:
   * one number cell holding both sides of a context line, and the anchored
   * line's number span the reconciler finds its row by.
   */
</script>

<table>
  <tbody>
    <tr class="diff-line">
      <td class="diff-line-num"
        ><span data-line-old-num="1">1</span><span data-line-new-num="1">1</span></td
      >
      <td class="diff-line-content">one</td>
    </tr>
  </tbody>
</table>
