<div class='content-wrapper post-related pool-reader'>
    <header>
        <h1>Reading <a href='<%= ctx.formatClientLink("pool", ctx.pool.id) %>'><%- ctx.pool.names[0] %></a></h1>
    </header>

    <ul class='related-scroll'>
        <% for (let post of ctx.posts) { %>
            <li>
                <header>
                    <a href='<%= ctx.getPostUrl(post.id, ctx.parameters) %>'>Post #<%- post.id %></a>
                    <span class='details'><%- post.canvasWidth %>&times;<%- post.canvasHeight %> &middot; <%- ctx.makeFileSize(post.fileSize) %></span>
                </header>
                <% if (post.type === 'video') { %>
                    <video controls preload='metadata' src='<%- post.contentUrl %>'></video>
                <% } else if (post.type === 'flash') { %>
                    <a href='<%= ctx.getPostUrl(post.id, ctx.parameters) %>'><%= ctx.makeThumbnail(post.thumbnailUrl) %></a>
                <% } else { %>
                    <a href='<%= ctx.getPostUrl(post.id, ctx.parameters) %>'><img alt='Post #<%- post.id %>' loading='lazy' src='<%- post.contentUrl %>'/></a>
                <% } %>
            </li>
        <% } %>
    </ul>
</div>
