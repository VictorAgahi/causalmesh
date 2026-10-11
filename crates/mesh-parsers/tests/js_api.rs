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
