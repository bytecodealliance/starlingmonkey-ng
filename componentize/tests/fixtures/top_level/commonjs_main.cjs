const path = require('node:path');
const { value } = require('./lib/value.cjs');
exports.greeting = () => `${path.join('a', 'b')} ${value}`;
