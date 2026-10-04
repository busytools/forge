/**
 * The worklet globals `lib.dom` does not carry.
 *
 * `AudioWorkletProcessor`, `registerProcessor` and the `process` signature
 * live in the worklet's own global scope, which TypeScript models nowhere -
 * so `mic-worklet.js` reads as calls on unresolvable types until they are
 * declared here. Only what that file uses is declared.
 */

declare abstract class AudioWorkletProcessor {
  readonly port: MessagePort;
  constructor();
}

declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;
