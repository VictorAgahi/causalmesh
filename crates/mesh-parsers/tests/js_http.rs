//! HTTP routes in JavaScript / TypeScript: verb registrations (Express, Fastify, Koa,
//! Hono, Elysia, h3, hapi, …) with prefixes from groups, plugins and in-file mounts,
//! and decorated controllers (NestJS, routing-controllers, tsoa, inversify).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use mesh_core::NodeKind;
use mesh_parsers::{FileIndex, PolyglotIndexer};
use std::path::Path;

fn index(file: &str, code: &str) -> FileIndex {
    let fi = PolyglotIndexer::extract(Path::new(file), code, 0);
    assert!(!fi.parse_failed, "{file} must parse");
    fi
}

/// The `METHOD /path` signature of every HTTP endpoint, sorted.
fn routes(fi: &FileIndex) -> Vec<String> {
    let mut out: Vec<String> = fi
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::HttpEndpoint)
        .map(|n| n.signature.as_deref().unwrap_or("").to_string())
        .collect();
    out.sort();
    out
}

fn r(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn express_verbs_handlers_and_catch_all() {
    let fi = index(
        "api/app.ts",
        "import express from 'express';\n\
         const app = express();\n\
         app.get('/users', listUsers);\n\
         app.post('/users', (req, res) => res.send('ok'));\n\
         app.delete('/users/:id', auth, remove);\n\
         app.all('*', notFound);\n\
         app.get(['/a', '/b'], both);\n",
    );
    assert_eq!(
        routes(&fi),
        r(&[
            "ALL *",
            "DELETE /users/:id",
            "GET /a",
            "GET /b",
            "GET /users",
            "POST /users"
        ])
    );
}

#[test]
fn express_router_chains_and_in_file_mounts_compose_prefixes() {
    let fi = index(
        "api/routes.js",
        "const express = require('express');\n\
         const app = express();\n\
         const v1 = express.Router();\n\
         const users = express.Router();\n\
         users.get('/:id', show);\n\
         users.route('/').get(list).post(create);\n\
         v1.use('/users', users);\n\
         app.use('/v1', v1);\n",
    );
    assert_eq!(
        routes(&fi),
        r(&["GET /v1/users", "GET /v1/users/:id", "POST /v1/users"])
    );
}

#[test]
fn fastify_plugins_prefixes_and_route_objects() {
    let fi = index(
        "api/server.ts",
        "import Fastify from 'fastify';\n\
         const fastify = Fastify();\n\
         fastify.get('/ping', { schema: {} }, async () => 'pong');\n\
         fastify.register(async (instance) => {\n\
           instance.get('/inner', handler);\n\
           instance.post('/inner', handler);\n\
         }, { prefix: '/api' });\n\
         fastify.route({ method: ['GET', 'POST'], url: '/multi', handler });\n",
    );
    assert_eq!(
        routes(&fi),
        r(&[
            "GET /api/inner",
            "GET /multi",
            "GET /ping",
            "POST /api/inner",
            "POST /multi"
        ])
    );
}

#[test]
fn koa_router_prefix_option() {
    let fi = index(
        "api/koa.ts",
        "import Router from '@koa/router';\n\
         const router = new Router({ prefix: '/api' });\n\
         router.get('/users', ctx => {});\n\
         router.put('/users/:id', update);\n",
    );
    assert_eq!(routes(&fi), r(&["GET /api/users", "PUT /api/users/:id"]));
}

#[test]
fn hono_base_path_on_and_mounted_apps() {
    let fi = index(
        "edge/app.ts",
        "import { Hono } from 'hono';\n\
         const app = new Hono();\n\
         const api = app.basePath('/api');\n\
         api.get('/users', (c) => c.json([]));\n\
         app.on('GET', '/health', (c) => c.text('ok'));\n\
         app.on(['PUT', 'DELETE'], '/things', handler);\n\
         const books = new Hono();\n\
         books.get('/', list);\n\
         app.route('/books', books);\n",
    );
    assert_eq!(
        routes(&fi),
        r(&[
            "DELETE /things",
            "GET /api/users",
            "GET /books",
            "GET /health",
            "PUT /things"
        ])
    );
}

#[test]
fn elysia_groups_and_hapi_route_objects_h3_and_restify() {
    let elysia = index(
        "edge/elysia.ts",
        "import { Elysia } from 'elysia';\n\
         new Elysia().group('/api', (app) => app.get('/users', () => []).post('/users', create));\n",
    );
    assert_eq!(routes(&elysia), r(&["GET /api/users", "POST /api/users"]));

    let hapi = index(
        "api/hapi.js",
        "const Hapi = require('@hapi/hapi');\n\
         const server = Hapi.server({ port: 3000 });\n\
         server.route({ method: 'GET', path: '/h', handler });\n\
         server.route([{ method: 'POST', path: '/a', handler }, { method: ['PUT', 'PATCH'], path: '/b', handler }]);\n",
    );
    assert_eq!(
        routes(&hapi),
        r(&["GET /h", "PATCH /b", "POST /a", "PUT /b"])
    );

    let restify = index(
        "api/restify.js",
        "const restify = require('restify');\nconst server = restify.createServer();\n\
         server.get('/echo/:name', respond);\nserver.del('/echo/:name', remove);\n",
    );
    assert_eq!(
        routes(&restify),
        r(&["DELETE /echo/:name", "GET /echo/:name"])
    );
}

#[test]
fn client_calls_and_headers_in_a_serving_file_are_not_routes() {
    let fi = index(
        "api/bff.ts",
        "import express from 'express';\nimport axios from 'axios';\nimport request from 'supertest';\n\
         const app = express();\n\
         app.get('/real', async (req, res) => {\n\
           const data = await axios.get('/upstream', config);\n\
           res.get('Content-Type');\n\
           cache.get('/cached');\n\
           await request(app).get('/under-test');\n\
           res.send(data);\n\
         });\n",
    );
    assert_eq!(routes(&fi), r(&["GET /real"]));
}

#[test]
fn nothing_is_a_route_without_a_framework_import() {
    let fi = index(
        "web/store.ts",
        "export function f(store, map) { store.get('/a', handler); map.post('/b', cb); }\n",
    );
    assert!(routes(&fi).is_empty());
}

#[test]
fn nestjs_controllers_state_the_full_route_in_the_signature() {
    let fi = index(
        "svc/users.controller.ts",
        "import { Controller, Get, Post, Delete } from '@nestjs/common';\n\
         const Routes = { Search: 'search' } as const;\n\
         @Controller('users')\n\
         export class UsersController {\n\
           @Get(':id') find() {}\n\
           @Post() create() {}\n\
           @Get(Routes.Search) search() {}\n\
           @Delete('/:id/') remove() {}\n\
         }\n\
         @Controller({ path: 'v2/items', version: '2' })\n\
         export class ItemsController { @Get() all() {} }\n\
         @Controller()\n\
         export class RootController { @Get('health') health() {} }\n",
    );
    let sig = |name: &str| {
        fi.nodes
            .iter()
            .find(|n| n.name == name && n.kind == NodeKind::HttpEndpoint)
            .and_then(|n| n.signature.as_deref().map(str::to_string))
    };
    assert_eq!(sig("find").as_deref(), Some("GET /users/:id"));
    assert_eq!(sig("create").as_deref(), Some("POST /users"));
    assert_eq!(sig("search").as_deref(), Some("GET /users/search"));
    assert_eq!(sig("remove").as_deref(), Some("DELETE /users/:id"));
    assert_eq!(sig("all").as_deref(), Some("GET /v2/items"));
    assert_eq!(sig("health").as_deref(), Some("GET /health"));
}

#[test]
fn routing_controllers_tsoa_and_inversify_controllers() {
    let rc = index(
        "svc/rc.ts",
        "import { JsonController, Get } from 'routing-controllers';\n\
         @JsonController('/users')\nexport class U { @Get('/:id') one() {} }\n",
    );
    assert_eq!(routes(&rc), r(&["GET /users/:id"]));
    let tsoa = index(
        "svc/tsoa.ts",
        "import { Route, Get, Post } from 'tsoa';\n\
         @Route('users')\nexport class U { @Get('{id}') one() {} @Post() add() {} }\n",
    );
    assert_eq!(routes(&tsoa), r(&["GET /users/{id}", "POST /users"]));
    let inversify = index(
        "svc/inv.ts",
        "import { controller, httpGet, httpPut } from 'inversify-express-utils';\n\
         @controller('/orders')\nexport class O { @httpGet('/:id') one() {} @httpPut('/:id') upd() {} }\n",
    );
    assert_eq!(
        routes(&inversify),
        r(&["GET /orders/:id", "PUT /orders/:id"])
    );
}

#[test]
fn route_registrations_belong_to_the_declaration_that_makes_them() {
    let fi = index(
        "api/setup.ts",
        "import express from 'express';\n\
         export function mount(app: express.Express) {\n\
           app.get('/a', a);\n\
         }\n",
    );
    // `app` is a typed parameter: the file builds no app, the import and the shape gate.
    assert_eq!(routes(&fi), r(&["GET /a"]));
}

// ---- review round 1 (PR #83) --------------------------------------------------------------

#[test]
fn chained_constructor_idioms_register_routes_even_with_other_apps_in_the_file() {
    let fi = index(
        "edge/chains.ts",
        "import { Elysia } from 'elysia';
import { Hono } from 'hono';
import Fastify from 'fastify';
         export const a = new Elysia({ prefix: '/p' }).get('/a', h).post('/b', h);
         export const b = new Hono().get('/c', h).post('/d', h);
         export const c = Fastify().get('/e', h);
         const app = new Hono();
app.get('/f', h);
",
    );
    assert_eq!(
        routes(&fi),
        r(&[
            "GET /c",
            "GET /e",
            "GET /f",
            "GET /p/a",
            "POST /d",
            "POST /p/b"
        ])
    );
}

#[test]
fn a_router_name_reused_in_two_functions_is_two_routers() {
    let fi = index(
        "api/two.ts",
        "import express from 'express';
         const app = express();
         export function users() {
           const router = express.Router();
           router.get('/', list);
           app.use('/users', router);
         }
         export function orders() {
           const router = express.Router();
           router.get('/', list);
           app.use('/orders', router);
         }
",
    );
    assert_eq!(routes(&fi), r(&["GET /orders", "GET /users"]));
}

#[test]
fn a_router_mounted_under_two_prefixes_serves_both_and_a_cycle_terminates() {
    let fi = index(
        "api/versions.ts",
        "import express from 'express';
         const app = express();
         const shared = express.Router();
         shared.get('/x', h);
         app.use('/v1', shared);
         app.use('/v2', shared);
         const a = express.Router();
const b = express.Router();
         a.use('/b', b);
b.use('/a', a);
b.get('/z', h);
",
    );
    let got = routes(&fi);
    assert!(
        got.contains(&"GET /v1/x".to_string()) && got.contains(&"GET /v2/x".to_string()),
        "{got:?}"
    );
    assert!(got.iter().any(|g| g.ends_with("/z")), "{got:?}");
}

#[test]
fn clients_are_not_routes_even_when_the_file_builds_no_app() {
    let fi = index(
        "web/client.ts",
        "import { HTTPException } from 'hono/http-exception';
import got from 'got';
         const client = got.extend({});
         export class S {
           constructor(private http: HttpClient) {}
           f() { client.get('/x', opts); return this.http.get('/api/u', opts); }
         }
",
    );
    assert!(routes(&fi).is_empty(), "{:?}", routes(&fi));
}

#[test]
fn koa_named_routes_and_hapi_wildcard_method() {
    let koa = index(
        "api/named.ts",
        "import Router from '@koa/router';
const router = new Router();
         router.get('user', '/users/:id', show);
",
    );
    assert_eq!(routes(&koa), r(&["GET /users/:id"]));
    let hapi = index(
        "api/any.js",
        "const Hapi = require('@hapi/hapi');
const server = Hapi.server();
         server.route({ method: '*', path: '/any', handler });
",
    );
    assert_eq!(routes(&hapi), r(&["ALL /any"]));
}

#[test]
fn a_verb_named_decorator_from_a_non_http_library_is_not_a_route() {
    let fi = index(
        "svc/cfg.ts",
        "import { Get, All } from 'some-config-lib';
         export class Cfg { @Get('x') a() {} @All() b() {} }
",
    );
    let signatures: Vec<String> = fi
        .nodes
        .iter()
        .filter_map(|n| n.signature.as_deref())
        .filter(|s| s.starts_with("GET /") || s.starts_with("ALL /"))
        .map(str::to_string)
        .collect();
    assert!(signatures.is_empty(), "{signatures:?}");
}

#[test]
fn module_names_are_matched_on_boundaries() {
    // "honor", "<h3>" and "expression" name no framework: no analysis, no routes.
    let fi = index(
        "web/prose.ts",
        "export function f(store) { /* honor the expression */ store.get('/a', cb); return '<h3>x</h3>'; }
",
    );
    assert!(routes(&fi).is_empty());
}
