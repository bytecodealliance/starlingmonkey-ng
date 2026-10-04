// The entry of this app's component in the Spin application `scripts/build-spin-app.sh` builds:
// the app, mounted at `/rest-api`, the route Spin serves it at.
import express from 'express';
import app from './app.js';

const outer = express();
outer.disable('x-powered-by');
outer.use('/rest-api', app);
outer.listen(Number(process.env.PORT ?? 3000));
