// Run the G2 notification shim in a real JavaScript engine.
//
// `web/notify.rs` builds `window.Notification` as a string and every Rust
// test that looks at it searches TEXT. Text search cannot tell a working
// shim from one that does not parse, and a shim that does not parse fails
// silently inside a page. This runs it.
//
//   EAVE_DUMP_SHIM=/tmp/shim.js cargo test -p eave-app web::notify
//   node scripts/check-notify-shim.mjs /tmp/shim.js
//
// Deliberately NOT part of `cargo test`: the suite should not require node.
// The load-bearing assertion is the third one — G2 exists because `onshow`
// fired for notifications that never displayed, so constructing one must
// raise nothing at all until eave says it showed.

import { readFileSync } from 'node:fs';
const src = readFileSync(process.argv[2], 'utf8');
let fails = 0;
const ok = (name, cond) => { console.log((cond ? 'ok   ' : 'FAIL ') + name); if (!cond) fails++; };
const sleep = () => new Promise(r => setTimeout(r, 0));

// --- 1. the honest path: eave is listening
let posted = [];
globalThis.__eaveNotify = (s) => posted.push(JSON.parse(s));
(0, eval)(src);

let shown = 0, errored = 0, closed = 0;
const n = new Notification('Build done', { body: '3 tests', tag: 'build' });
n.onshow = () => shown++;
n.onerror = () => errored++;
n.addEventListener('close', () => closed++);

await sleep();
ok('nothing is posted as an origin', !('origin' in posted[0]));
ok('the post carries title/body/tag', posted[0].title === 'Build done' && posted[0].body === '3 tests' && posted[0].tag === 'build');
ok('G2: onshow has NOT fired merely from constructing', shown === 0);

__eaveNotifyAck(posted[0].seq, 'show');
ok('onshow fires when eave says it showed', shown === 1);
__eaveNotifyAck(posted[0].seq, 'close');
ok('close reaches an addEventListener listener', closed === 1);
__eaveNotifyAck(posted[0].seq, 'show');
ok('a closed notification is forgotten, not re-shown', shown === 1);

// --- 2. permission
const p = Notification.requestPermission();
ok('permission starts at default', Notification.permission === 'default');
__eaveNotifyAck(0, 'permission', 'granted');
ok('permission is readable synchronously after the ack', Notification.permission === 'granted');
ok('requestPermission resolved with it', (await p) === 'granted');

// --- 3. installing twice is a no-op
const before = Notification;
(0, eval)(src);
ok('re-install leaves the first class in place', Notification === before);

// --- 4. eave is NOT listening (binding missing)
delete globalThis.__eaveNotify;
delete globalThis.__eaveNotifyAck;
delete globalThis.Notification;
(0, eval)(src);
let e2 = 0, s2 = 0;
const n2 = new Notification('orphan');
n2.onerror = () => e2++;
n2.onshow = () => s2++;
await sleep();
ok('no binding: error fires', e2 === 1);
ok('no binding: onshow NEVER fires', s2 === 0);
ok('no binding: requestPermission resolves denied', (await Notification.requestPermission()) === 'denied');

// --- 5. callback form still works
delete globalThis.__eaveNotifyAck; delete globalThis.Notification;
globalThis.__eaveNotify = () => {};
(0, eval)(src);
let cbArg = null;
Notification.requestPermission(v => { cbArg = v; });
__eaveNotifyAck(0, 'permission', 'denied');
await sleep();
ok('the legacy callback form is answered too', cbArg === 'denied');

console.log(fails === 0 ? '\nALL PASS' : `\n${fails} FAILED`);
process.exit(fails ? 1 : 0);
