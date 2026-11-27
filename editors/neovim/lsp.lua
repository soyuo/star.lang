local command = vim.env.STAR_LSP or "star"
local args = command == "star" and { "lsp" } or {}

vim.api.nvim_create_autocmd("FileType", {
  pattern = "star",
  callback = function(event)
    vim.lsp.start({
      name = "star-lsp",
      cmd = vim.list_extend({ command }, args),
      root_dir = vim.fs.root(event.buf, { ".git" }) or vim.fn.getcwd(),
      on_attach = function(client, buffer)
        if client:supports_method("textDocument/foldingRange") and vim.lsp.foldexpr then
          local window = vim.fn.bufwinid(buffer)
          if window ~= -1 then
            vim.wo[window].foldmethod = "expr"
            vim.wo[window].foldexpr = "v:lua.vim.lsp.foldexpr()"
          end
        end
      end,
    })
  end,
})
