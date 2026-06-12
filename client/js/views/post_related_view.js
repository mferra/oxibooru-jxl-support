"use strict";

const views = require("../util/views.js");

const template = views.getTemplate("post-related");

class PostRelatedView {
    constructor(ctx) {
        this._hostNode = document.getElementById("content-holder");
        views.replaceContent(this._hostNode, template(ctx));
        views.syncScrollPosition();
    }
}

module.exports = PostRelatedView;
