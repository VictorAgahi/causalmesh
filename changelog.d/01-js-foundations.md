### JS/TS parser: foundations (7.1.0 stack, PR 1/7)
- **JavaScript has its own grammar** (`tree-sitter-javascript`, `LanguageKind::JavaScript`): `.js`, `.jsx`,
  `.mjs`, `.cjs` no longer go through the TSX grammar, which misparses plain JS. `.tsx` stays on TSX.
- **CommonJS and dynamic imports are dependencies**: `require('x')`, `import('x')` (anywhere in the file),
  `import x = require('y')`, destructured `const { a } = require('./m')` (named, attributed where used).
  `const express = require('express')` is an import, no longer a declaration named `express`.
  Computed specifiers (`require(name)`, template literals) are ignored.
- **More declarations**: `declare function/const/class/enum/type` (`.d.ts`), `namespace`/`module` blocks,
  anonymous `export default` (object, function, class, call given an inline definition) named after the
  file (`index.*` -> its directory), CommonJS `module.exports = …`, `exports.foo = …`, `module.exports.foo = …`.
- **Decapitation** also strips `function_expression` and generator bodies.
