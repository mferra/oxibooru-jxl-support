"use strict";

const api = require("../api.js");
const topNavigation = require("../models/top_navigation.js");
const Pool = require("../models/pool.js");
const Post = require("../models/post.js");
const PoolReaderView = require("../views/pool_reader_view.js");
const EmptyView = require("../views/empty_view.js");

class PoolReaderController {
    constructor(ctx) {
        if (!api.hasPrivilege("pool_view") || !api.hasPrivilege("post_view")) {
            this._view = new EmptyView();
            this._view.showError(
                "You don't have privileges to view this pool."
            );
            return;
        }

        topNavigation.activate("pools");

        Pool.get(ctx.parameters.id)
            .then((pool) =>
                Promise.all([
                    pool,
                    ...pool.posts.map((post) => Post.get(post.id)),
                ])
            )
            .then(
                (results) => {
                    const [pool, ...posts] = results;
                    topNavigation.setTitle("Reading pool " + pool.names[0]);
                    this._view = new PoolReaderView({
                        pool: pool,
                        posts: posts,
                        parameters: ctx.parameters,
                    });
                },
                (error) => {
                    this._view = new EmptyView();
                    this._view.showError(error.message);
                }
            );
    }
}

module.exports = (router) => {
    router.enter(["pool", ":id", "reader"], (ctx, next) => {
        ctx.controller = new PoolReaderController(ctx);
    });
};
