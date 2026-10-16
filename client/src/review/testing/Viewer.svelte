<script lang="ts">
  /**
   * The diff viewer, stood in for in tests: the shapes a live page renders,
   * which is the whole of the canvas's contract with it.
   *
   * jsdom cannot mount the real viewer - its text measurement allocates
   * without bound there - so the table below is a capture of the unified
   * shape, the same ones `diff.dom.test.ts` reads the page's decisions off:
   * one number cell holding both sides of a context line, and the anchored
   * line's number span the reconciler finds its row by. Two rows, because a
   * card may be anchored somewhere other than line 1 - a moved thread's
   * resolution draws it away from where the comment was written - and a
   * third, line 9, that no hunk in the tests carries: the shape an expanded
   * gap's line draws, which takes no comment door.
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
    <tr class="diff-line">
      <td class="diff-line-num"
        ><span data-line-old-num="4">4</span><span data-line-new-num="4">4</span></td
      >
      <td class="diff-line-content">four</td>
    </tr>
    <tr class="diff-line">
      <td class="diff-line-num"
        ><span data-line-old-num="9">9</span><span data-line-new-num="9">9</span></td
      >
      <td class="diff-line-content">nine</td>
    </tr>
  </tbody>
</table>
