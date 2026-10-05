// `node:diagnostics_channel` holds a channel without subscribers weakly, and one with subscribers
// or bound stores strongly. Needs `--expose-gc`.

import dc from "node:diagnostics_channel";
import { eq } from "./harness.mjs";

const tick = () => new Promise((resolve) => setTimeout(resolve, 10));

let unreferenced = dc.channel("unreferenced");
const unreferencedRef = new WeakRef(unreferenced);
eq(dc.channel("unreferenced"), unreferenced, "the same channel while it is referenced");
unreferenced = null;

let subscribed = dc.channel("subscribed");
const onMessage = () => {};
subscribed.subscribe(onMessage);
const subscribedRef = new WeakRef(subscribed);
subscribed = null;

for (let i = 0; i < 100; i++) dc.channel(`request:${i}`);
await tick();
gc();
await tick();

eq(unreferencedRef.deref(), undefined, "a channel nothing references is collected");
eq(subscribedRef.deref() !== undefined, true, "a channel with subscribers is kept");
eq(dc.channel("subscribed"), subscribedRef.deref(), "the kept channel stays the realm's channel");
eq(dc.hasSubscribers("subscribed"), true, "hasSubscribers of the kept channel");
eq(dc.hasSubscribers("unreferenced"), false, "hasSubscribers of a collected channel's name");
dc.channel("subscribed").unsubscribe(onMessage);
