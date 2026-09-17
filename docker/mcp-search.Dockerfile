# mcp-searxng with native streamable HTTP transport.
# Talks to the SearXNG container over the compose network.
FROM node:22-alpine

RUN npm install -g mcp-searxng@latest \
    && npm cache clean --force

USER node

CMD ["mcp-searxng"]
