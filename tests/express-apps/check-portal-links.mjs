#!/usr/bin/env node
// Checks that every link and form on the Koa portal's pages works against the deployment at a
// base URL, such as the Spin application `scripts/build-spin-app.sh` builds:
//
//   deps/spin-bin/spin up --from target/spin-app/spin.toml --allow-transient-write
//   node tests/express-apps/check-portal-links.mjs http://127.0.0.1:3000
//
// It loads the portal's home page and each app's page, then requests every link of those pages
// and submits every form with its fields' values, uploading a small text file for file fields.
// Redirects are followed. A link or form works if its final response has a status below 400, or
// 409, which forms that create something only once, such as a registration, return when submitted
// again. Exits with status 1 if anything doesn't work.

const base = (process.argv[2] ?? 'http://127.0.0.1:3000').replace(/\/$/, '');

const decode = (s) =>
  s.replace(/&#x([0-9a-f]+);/gi, (_, hex) => String.fromCodePoint(parseInt(hex, 16))).replace(/&amp;/g, '&').replace(/&quot;/g, '"');

async function get(path) {
  const res = await fetch(base + path);
  return { status: res.status, text: await res.text(), url: res.url };
}

const pages = ['/', ...(await (await fetch(`${base}/api/apps`)).json()).map((app) => `/apps/${app.name}`)];
const links = new Set();
const forms = new Map();
for (const page of pages) {
  const { status, text } = await get(page);
  if (status !== 200) throw new Error(`${page} answered ${status}`);
  for (const [, href] of text.matchAll(/<a href="([^"]+)"/g)) links.add(decode(href));
  for (const [form, attributes] of text.matchAll(/<form([^>]*)>[\s\S]*?<\/form>/g)) {
    const attribute = (name) => decode(new RegExp(`${name}="([^"]*)"`).exec(attributes)?.[1] ?? '');
    // A select's value is its first option's.
    const fields = [...form.matchAll(/<(input|textarea|select)([^>]*)>(?:\s*<option value="([^"]*)")?/g)].map(
      ([, tag, field, option]) => ({
        name: decode(/name="([^"]*)"/.exec(field)?.[1] ?? ''),
        type: /type="([^"]*)"/.exec(field)?.[1] ?? 'text',
        value: decode((tag === 'select' ? option : /value="([^"]*)"/.exec(field)?.[1]) ?? ''),
      }),
    );
    const key = `${attribute('method').toUpperCase()} ${attribute('action')}`;
    forms.set(key, { method: attribute('method').toUpperCase(), action: attribute('action'), enctype: attribute('enctype'), fields });
  }
}

let failed = 0;
const report = (ok, what, status, url) => {
  if (!ok) failed++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${status} ${what}${url && !url.endsWith(what) ? ` -> ${url.slice(base.length)}` : ''}`);
};

for (const link of [...links].sort()) {
  if (link.startsWith('http') || link.startsWith('#') || link.startsWith('/#')) continue;
  const { status, url } = await get(link);
  report(status < 400, `GET ${link}`, status, url);
}

// Forms are submitted in the order of the pages, which registers a user before logging in.
for (const [key, form] of forms) {
  const values = form.fields.filter((field) => field.name);
  let res;
  if (form.method === 'GET') {
    const query = new URLSearchParams(values.map((field) => [field.name, field.value]));
    res = await fetch(`${base}${form.action}?${query}`);
  } else {
    let body;
    if (form.enctype === 'multipart/form-data') {
      body = new FormData();
      for (const field of values) {
        if (field.type === 'file') body.append(field.name, new Blob(['uploaded from the portal check\n'], { type: 'text/plain' }), 'portal.txt');
        else body.append(field.name, field.value);
      }
    } else {
      // The feedback form's message is empty by default, and needs a few characters.
      body = new URLSearchParams(values.map((field) => [field.name, field.value || (field.name === 'name' ? 'Portal check' : 'Checked every form')]));
    }
    res = await fetch(base + form.action, { method: form.method, body });
  }
  await res.arrayBuffer();
  report(res.status < 400 || res.status === 409, key, res.status, res.url);
}

console.log(`${links.size} links and ${forms.size} forms checked, ${failed} failed`);
process.exit(failed ? 1 : 0);
