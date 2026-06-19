// JavaScript implementation of SeedSource.GetSeed (bv64 -> BigInt).
// Build/run:  dafny run --target:js SeedSourceDemo.dfy --input SeedSource.js
// (Dafny's JS runtime needs bignumber.js available, e.g. `npm install bignumber.js`.)
//
// The generated module does `let $module = SeedSource;`, i.e. it expects a
// pre-existing global `SeedSource`; we define it here with the GetSeed member.
let SeedSource = (function () {
  let $module = {};
  $module.GetSeed = function () {
    const crypto = require('crypto');
    return crypto.randomBytes(8).readBigUInt64LE(0);   // bv64 is a BigInt in Dafny's JS runtime
  };
  return $module;
})();
