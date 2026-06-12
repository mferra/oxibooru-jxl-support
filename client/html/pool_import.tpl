<div class='content-wrapper pool-import'>
    <h1>Import comic archive</h1>
    <form>
        <ul class='input'>
            <li class='archive-file'>
                <label for='archive-file'>CBZ file</label>
                <input type='file' id='archive-file' accept='.cbz,.zip,application/zip'/>
            </li>
            <li class='archive-url'>
                <%= ctx.makeTextInput({
                    text: 'or archive URL (fetched by the server)',
                    placeholder: 'http://',
                }) %>
            </li>
            <li class='name'>
                <%= ctx.makeTextInput({
                    text: 'Pool name (defaults to the file name)',
                }) %>
            </li>
            <li class='safety'>
                <%= ctx.makeSelect({
                    text: 'Safety for newly created posts',
                    name: 'safety',
                    keyValues: {safe: 'Safe', sketchy: 'Sketchy', unsafe: 'Unsafe'},
                    selectedKey: 'safe',
                }) %>
            </li>
        </ul>

        <p>Pages that already exist on the site are matched to their posts;
        the rest are uploaded as new posts. Importing can take several minutes
        for large archives.</p>

        <div class='messages'></div>

        <div class='buttons'>
            <input type='submit' class='save' value='Import'/>
        </div>
    </form>
</div>
