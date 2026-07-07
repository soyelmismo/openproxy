const crypto = require('crypto');
const token = 'op_live_test_dummy_token_for_e2e';
const hash = crypto.createHash('sha256').update(token).digest('hex');
console.log('Token hash:', hash);
