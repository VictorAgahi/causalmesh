### JS/TS parser: foundations (7.1.0 stack, PR 1/7)
- **JavaScript has its own grammar** (`tree-sitter-javascript`, `LanguageKind::JavaScript`): `.js`, `.jsx`,
  `.mjs`, `.cjs` no longer go through the TSX grammar, which misparses plain JS. `.tsx` stays on TSX.
  A `.js` file carrying Flow/Babel annotations (`type`, `interface`, `: number`) that does not parse cleanly
  as JavaScript is re-parsed as TSX when that parse is clean (indexing and decapitation), so such files
  keep their classes and types.
- **CommonJS and dynamic imports are dependencies**: `require('x')`, `import('x')` (anywhere in the file),
  `import x = require('y')`, destructured `const { a, b = 1 } = require('./m')` (named, attributed where used).
  `const express = require('express')` is an import, no longer a declaration named `express`. Computed
  specifiers (`require(name)`, template literals) and a `require` shadowed by a parameter are ignored.
- **More declarations**: `declare function/const/class/enum/type` and bodiless signatures in `.d.ts`,
  `namespace` / `module X` blocks and their members, anonymous `export default` (object, function, class,
  call given an inline definition) named after the file (`index.*` -> its directory), CommonJS
  `module.exports = …`, `exports.foo = …`, `module.exports.foo = …`, `exports = module.exports = …`
  and chained `a = b = …`. A repeated export is one node. Not covered: `export =`, `export import`,
  `module['exports']`, UMD/webpack wrappers, members of a string-named `declare module 'x'`.
- **gRPC client fields in JavaScript**: inherited fields (`^Base#field.method`) and own class fields
  resolve for `.js` as they do for `.ts`.
- **Decapitation** also strips `function_expression`, generators and class `static {}` blocks.
- **Index cache schema v10**: unchanged files are re-extracted once.
