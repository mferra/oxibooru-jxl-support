"use strict";

const events = require("../events.js");
const views = require("../util/views.js");

const template = views.getTemplate("pool-import");

class PoolImportView extends events.EventTarget {
    constructor(ctx) {
        super();

        this._hostNode = document.getElementById("content-holder");
        views.replaceContent(this._hostNode, template(ctx));

        views.decorateValidator(this._formNode);
        this._formNode.addEventListener("submit", (e) => this._evtSubmit(e));
    }

    clearMessages() {
        views.clearMessages(this._hostNode);
    }

    enableForm() {
        views.enableForm(this._formNode);
    }

    disableForm() {
        views.disableForm(this._formNode);
    }

    showSuccess(message) {
        views.showSuccess(this._hostNode, message);
    }

    showError(message) {
        views.showError(this._hostNode, message);
    }

    get _formNode() {
        return this._hostNode.querySelector("form");
    }

    get _fileFieldNode() {
        return this._formNode.querySelector("#archive-file");
    }

    get _urlFieldNode() {
        return this._formNode.querySelector(".archive-url input");
    }

    get _nameFieldNode() {
        return this._formNode.querySelector(".name input");
    }

    get _safetyFieldNode() {
        return this._formNode.querySelector(".safety select");
    }

    _evtSubmit(e) {
        e.preventDefault();
        this.dispatchEvent(
            new CustomEvent("submit", {
                detail: {
                    file: this._fileFieldNode.files[0] || null,
                    url: this._urlFieldNode.value.trim(),
                    name: this._nameFieldNode.value.trim(),
                    safety: this._safetyFieldNode.value,
                },
            })
        );
    }
}

module.exports = PoolImportView;
