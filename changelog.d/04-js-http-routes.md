### JS/TS parser: HTTP routes (7.1.0 stack, PR 4/7)
- **Route registrations become `HttpEndpoint` nodes** named and signed by method and full path
  (`GET /api/users/:id`): Express (`app.get`, `router.route('/p').get(h).post(h)`, `all`, path arrays),
  Fastify (`get(path, opts, handler)`, `route({ method, url })`, `register(plugin, { prefix })`), Koa
  (`@koa/router`, `new Router({ prefix })`), Hono (`basePath`, `on('GET', …)`, mounted apps), Elysia
  (`group('/api', app => …)`), h3, restify (`del`), polka, itty-router and hapi (`server.route({ method,
  path })`, arrays, method lists).
- **Prefixes are composed**: `group`, `register({ prefix })`, `new Router({ prefix })`, `basePath` and in-file
  mounts (`app.use('/v1', v1); v1.use('/users', users)`, Hono `app.route('/books', books)`), nested.
  A mount in *another* file is not followed.
- **Gated on the file and the receiver**, like the brokers: the file must load an HTTP framework and, when
  it builds apps or routers of it, the call must be on one of them (or on the router parameter of a
  `group` / `register` callback). `axios.get('/u', config)`, `res.get('Content-Type')`, `cache.get('/k')`
  and `request(app).get('/x')` are not routes; a receiver bound to an HTTP client library never is.
- **Decorated controllers state their full route in the signature**, as Python's do (the node keeps the
  method's name, so symbol lookups are unchanged): `@Controller('users')` + `@Get(':id')` ->
  `GET /users/:id` for NestJS (incl. `@Controller({ path })`, `@Controller()`, constants as paths),
  routing-controllers (`@JsonController`), tsoa (`@Route`, `{id}` kept as written) and
  inversify-express-utils (`@controller` / `@httpGet`, now recognised as endpoints).
- **Chained constructors and reused names**: `new Elysia({ prefix }).get(…).post(…)`, `new Hono().get(…)`,
  `Fastify().get(…)` and `Router().get(…)` register routes whatever else the file builds; a router variable
  reused in two functions is two routers, and one router mounted under two prefixes serves both (mount
  cycles stop; at most 4 prefixes are kept per router, in declaration order, so a router mounted thousands
  of times cannot fan out without bound). koa named routes (`router.get('user', '/users/:id', h)`) and hapi's `method: '*'` are read.
- **Fewer false positives**: with no app or router built in the file, a receiver must look like one (or be
  imported); `got.extend()` / `HttpClient` clients are not routes; a verb-named decorator imported from a
  non-HTTP library declares no route; framework names are matched as modules (`'hono'`), not inside
  "honor" or `<h3>`.
- **Known limits**: NestJS global prefixes and `@Version`, Express/Koa mounts across files, file-based routing
  (Next, Remix, SvelteKit, Nuxt, Astro: next PR), a Fastify plugin passed as a named function, an inherited
  controller base class's `@Get` (it lacks the child's `@Controller` prefix), `@Get(['a', 'b'])` (first path only) and HTTP client calls are not linked to the routes.
- Long method chains (more than ~30 links in one expression) are read up to a depth bound and silently stop; at most 5000 route nodes are recorded per file.
