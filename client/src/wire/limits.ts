/**
 * How much of something the wire carries, named once.
 *
 * Each of these is the client's side of a number the server's own fold decides,
 * and the two sides cannot share it - the server is Rust, and no import crosses.
 * So a client constant that draws one of these shapes imports it from here
 * rather than declaring a copy of its own, and each one names the file it
 * mirrors so a change on that side is a `grep` away rather than a surprise.
 */

/**
 * How many readings a take's meter keeps, which is the cap the server's own
 * fold holds them at: `METER_CELLS` in `crates/forge-server/src/composer.rs`.
 */
export const METER_CELLS = 120;
