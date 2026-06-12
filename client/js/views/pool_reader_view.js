"use strict";

const views = require("../util/views.js");

const template = views.getTemplate("pool-reader");

class PoolReaderView {
    constructor(ctx) {
        this._hostNode = document.getElementById("content-holder");
        views.replaceContent(this._hostNode, template(ctx));
        views.syncScrollPosition();
    }
}

module.exports = PoolReaderView;
