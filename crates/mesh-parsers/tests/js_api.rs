//! tRPC procedures and GraphQL resolvers as `HttpEndpoint` operations.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use mesh_core::NodeKind;
use mesh_parsers::{FileIndex, PolyglotIndexer};
use std::path::Path;

fn index(file: &str, code: &str) -> FileIndex {
    let fi = PolyglotIndexer::extract(Path::new(file), code, 0);
    assert!(!fi.parse_failed, "{file} must parse");
    fi
}

fn ops(fi: &FileIndex) -> Vec<String> {
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
fn trpc_procedures_with_nested_routers() {
    let fi = index(
        "server/router.ts",
        "import { initTRPC } from '@trpc/server';\nimport { z } from 'zod';\n\
         const t = initTRPC.create();\n\
         const publicProcedure = t.procedure;\n\
         export const appRouter = t.router({\n\
           getUser: publicProcedure.input(z.string()).query(({ input }) => input),\n\
           createUser: publicProcedure.input(z.object({})).mutation(() => 1),\n\
           user: t.router({\n\
             get: publicProcedure.query(() => 1),\n\
             onChange: publicProcedure.subscription(() => null),\n\
           }),\n\
           health: t.procedure.query(() => 'ok'),\n\
           other: client.query(() => 1),\n\
         });\n",
    );
    assert_eq!(
        ops(&fi),
        r(&[
            "TRPC mutation createUser",
            "TRPC query getUser",
            "TRPC query health",
            "TRPC query user.get",
            "TRPC subscription user.onChange",
        ])
    );
}

#[test]
fn nestjs_graphql_resolvers_and_type_graphql() {
    let fi = index(
        "svc/users.resolver.ts",
        "import { Resolver, Query, Mutation, Args } from '@nestjs/graphql';\n\
         @Resolver(() => User)\n\
         export class UsersResolver {\n\
           @Query(() => [User]) users() { return []; }\n\
           @Query(() => User, { name: 'user' }) findOne(@Args('id') id: string) { return null; }\n\
           @Mutation(() => User) createUser() { return null; }\n\
         }\n",
    );
    assert_eq!(
        ops(&fi),
        r(&[
            "GRAPHQL Mutation createUser",
            "GRAPHQL Query user",
            "GRAPHQL Query users"
        ])
    );
    let tgql = index(
        "svc/tg.ts",
        "import { Resolver, Query } from 'type-graphql';\n@Resolver()\nexport class R { @Query(() => String) hello() { return 'x'; } }\n",
    );
    assert_eq!(ops(&tgql), r(&["GRAPHQL Query hello"]));
}

#[test]
fn the_rest_query_decorator_is_not_a_graphql_operation() {
    let fi = index(
        "svc/rest.controller.ts",
        "import { Controller, Get, Query } from '@nestjs/common';\n\
         @Controller('items')\nexport class C { @Get() list(@Query() q: object) { return []; } }\n",
    );
    assert_eq!(ops(&fi), r(&["GET /items"]));
}

#[test]
fn apollo_style_resolver_maps() {
    let fi = index(
        "server/resolvers.ts",
        "import { ApolloServer } from '@apollo/server';\n\
         export const resolvers = {\n\
           Query: { users: () => [], user(_: unknown, args: { id: string }) { return null; } },\n\
           Mutation: { addUser: () => 1 },\n\
           User: { posts: () => [] },\n\
         };\n",
    );
    assert_eq!(
        ops(&fi),
        r(&[
            "GRAPHQL Mutation addUser",
            "GRAPHQL Query user",
            "GRAPHQL Query users"
        ])
    );
    // A `Query:` map in a file that has nothing to do with GraphQL is data.
    let data = index(
        "web/data.ts",
        "export const labels = { Query: { users: 'Users' } };\n",
    );
    assert!(ops(&data).is_empty());
}

// ---- review round 1 (PR #86) ------------------------------------------------------------

#[test]
fn trpc_routers_mounted_through_variables_compose_the_path() {
    let fi = index(
        "server/api/root.ts",
        "import { createTRPCRouter, publicProcedure } from '~/server/api/trpc';\n\
         export const postRouter = createTRPCRouter({\n\
           hello: publicProcedure.query(() => 'hi'),\n\
           inner: createTRPCRouter({ deep: publicProcedure.query(() => 1) }),\n\
         });\n\
         export const appRouter = createTRPCRouter({ post: postRouter, ping: publicProcedure.query(() => 'pong') });\n",
    );
    assert_eq!(
        ops(&fi),
        r(&[
            "TRPC query ping",
            "TRPC query post.hello",
            "TRPC query post.inner.deep",
        ])
    );
}

#[test]
fn procedure_looking_chains_outside_trpc_files_are_not_procedures() {
    let fi = index(
        "db/procs.ts",
        "export const handlers = { run: storedProcedure.query(sql), other: db.procedureCache.query(x) };\n",
    );
    assert!(ops(&fi).is_empty());
}

#[test]
fn graphql_maps_need_a_resolvers_object_and_a_graphql_library() {
    // Apollo Client cache policies are not resolvers.
    let client = index(
        "web/cache.ts",
        "import { InMemoryCache } from '@apollo/client';\n\
         export const cache = new InMemoryCache({ typePolicies: { Query: { fields: { todos: { merge: true } } }, Mutation: { fields: {} } } });\n",
    );
    assert!(ops(&client).is_empty(), "{:?}", ops(&client));
    // Data that happens to be keyed `Query`, even in a file that imports graphql.
    let data = index(
        "web/i18n.ts",
        "import { print } from 'graphql';\nexport const i18n = { Query: { title: 'Search' } };\nexport const admin = { x: { Query: { secret: 1 } } };\n",
    );
    assert!(ops(&data).is_empty());
    // A prose mention is no import.
    let prose = index(
        "web/yoga.ts",
        "// yoga class schedule\nexport const resolvers = { Query: { title: 'x' } };\n",
    );
    assert!(ops(&prose).is_empty());
    // The `resolvers:` option of a server.
    let server = index(
        "server/index.ts",
        "import { ApolloServer } from '@apollo/server';\n\
         const server = new ApolloServer({ typeDefs, resolvers: { Query: { users: () => [] } } });\n",
    );
    assert_eq!(ops(&server), r(&["GRAPHQL Query users"]));
}

#[test]
fn schema_first_and_aliased_nest_graphql_decorators() {
    let fi = index(
        "svc/author.resolver.ts",
        "import { Resolver, Query as Q, Mutation } from '@nestjs/graphql';\n\
         @Resolver('Author')\nexport class AuthorResolver {\n\
           @Q('author') getAuthor() { return null; }\n\
           @Mutation() upvotePost() { return null; }\n\
         }\n",
    );
    assert_eq!(
        ops(&fi),
        r(&["GRAPHQL Mutation upvotePost", "GRAPHQL Query author"])
    );
}

#[test]
fn api_scans_stay_cheap_on_big_files_without_a_graphql_or_trpc_mention() {
    // 14k methods and 9k `Query:` pairs in files that mention neither library.
    let mut class = String::from("export class Big {\n");
    for i in 0..14_000 {
        class.push_str(&format!("  m{i}() {{ return {i}; }}\n"));
    }
    class.push_str("}\n");
    let mut pairs = String::from("export const data = {\n");
    for i in 0..9_000 {
        pairs.push_str(&format!("  k{i}: {{ Query: {{ a{i}: 1 }} }},\n"));
    }
    pairs.push_str("};\n");
    for (name, code) in [("svc/big.ts", class), ("web/pairs.ts", pairs)] {
        let start = std::time::Instant::now();
        let fi = index(name, &code);
        assert!(ops(&fi).is_empty());
        assert!(
            start.elapsed().as_secs() < 8,
            "{name} took {:?}",
            start.elapsed()
        );
    }
}

#[test]
fn a_huge_resolver_map_is_capped_like_every_route_source() {
    let mut code = String::from(
        "import { ApolloServer } from '@apollo/server';\nexport const resolvers = {\n  Query: {\n",
    );
    for i in 0..20_000 {
        code.push_str(&format!("    f{i}: () => {i},\n"));
    }
    code.push_str("  },\n};\n");
    let start = std::time::Instant::now();
    let fi = index("server/huge.ts", &code);
    assert!(ops(&fi).len() <= 5000, "{} nodes", ops(&fi).len());
    assert!(start.elapsed().as_secs() < 15, "took {:?}", start.elapsed());
}
