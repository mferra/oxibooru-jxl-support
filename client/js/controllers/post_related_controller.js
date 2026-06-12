"use strict";

const topNavigation = require("../models/top_navigation.js");
const Post = require("../models/post.js");
const PostRelatedView = require("../views/post_related_view.js");
const BasePostController = require("./base_post_controller.js");
const EmptyView = require("../views/empty_view.js");

class PostRelatedController extends BasePostController {
    constructor(ctx) {
        super(ctx);

        Post.get(ctx.parameters.id)
            .then((post) =>
                Promise.all([
                    post,
                    ...post.relations.map((relation) => Post.get(relation.id)),
                ])
            )
            .then(
                (posts) => {
                    const post = posts[0];
                    topNavigation.setTitle(
                        "Related posts for post #" + ctx.parameters.id
                    );
                    this._view = new PostRelatedView({
                        post: post,
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
    router.enter(["post", ":id", "related"], (ctx, next) => {
        // restore parameters from history state
        if (ctx.state.parameters) {
            Object.assign(ctx.parameters, ctx.state.parameters);
        }
        ctx.controller = new PostRelatedController(ctx);
    });
};
