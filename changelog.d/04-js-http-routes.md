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
- **Known limits**: NestJS global prefixes and `@Version`, Express/Koa mounts across files, file-based routing
  (Next, Remix, SvelteKit, Nuxt, Astro: next PR) and HTTP client calls are not linked to the routes.
