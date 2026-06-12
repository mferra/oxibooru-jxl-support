"use strict";

const router = require("../router.js");
const api = require("../api.js");
const misc = require("../util/misc.js");
const uri = require("../util/uri.js");
const topNavigation = require("../models/top_navigation.js");
const PoolImportView = require("../views/pool_import_view.js");
const EmptyView = require("../views/empty_view.js");

class PoolImportController {
    constructor(ctx) {
        if (
            !api.hasPrivilege("pool_create") ||
            !api.hasPrivilege("post_create")
        ) {
            this._view = new EmptyView();
            this._view.showError(
                "You don't have privileges to import comic archives."
            );
            return;
        }

        topNavigation.activate("pools");
        topNavigation.setTitle("Import comic archive");

        this._view = new PoolImportView({});
        this._view.addEventListener("submit", (e) => this._evtImport(e));
    }

    _evtImport(e) {
        this._view.clearMessages();

        const { file, url, name, safety } = e.detail;
        if (!file && !url) {
            this._view.showError("Select a CBZ file or provide a URL.");
            return;
        }

        this._view.disableForm();
        const data = { safety: safety };
        if (name) {
            data.name = name;
        }

        let promise;
        if (file) {
            promise = api.postDirect(
                uri.formatApiLink("pool-from-archive"),
                data,
                { archive: file }
            );
        } else {
            data.archiveUrl = url;
            promise = api.post(uri.formatApiLink("pool-from-archive"), data);
        }

        promise.then(
            (response) => {
                misc.disableExitConfirmation();
                router.show(
                    uri.formatClientLink("pool", response.poolId, "reader")
                );
            },
            (error) => {
                this._view.showError(error.message);
                this._view.enableForm();
            }
        );
    }
}

module.exports = (router) => {
    router.enter(["pool", "import"], (ctx, next) => {
        ctx.controller = new PoolImportController(ctx);
    });
};
