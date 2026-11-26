local M = {}

local LABEL = "swiftspot"

local function open_pane()
  local listed = rex.session.list_blocks{}
  for _, block in ipairs((listed and listed.blocks) or {}) do
    if block.label == LABEL and block.placement == "placed" then
      return block.block_id
    end
  end
end

function M.setup(opts)
  opts = opts or {}
  local bin = opts.bin or "/Applications/SwiftSpot.app/Contents/Helpers/swiftspot"
  local keys = opts.keys or {}

  local function layout()
    return rex.layout.block{
      flavor = "com.superlogical.terminal.shell",
      label = LABEL,
      options = { command = { bin } },
    }
  end

  rex.action{
    name = "swiftspot",
    title = "Open SwiftSpot",
    description = "The SwiftSpot player beside this pane, or the open one.",
    category = "SwiftSpot",
    keywords = "spotify music player swiftspot",
    run = function(ctx)
      local open = open_pane()
      if open then
        local _, err = rex.session.focus_block{ block_id = open }
        if err ~= nil then error(err, 0) end
        return
      end
      if ctx.block_id then
        local split, err = rex.session.new_split{
          anchor_id = ctx.block_id,
          direction = "horizontal",
          side = "after",
          ratio = opts.ratio or 0.5,
          focus = true,
          layout = layout(),
        }
        if err ~= nil then error(err, 0) end
        return split
      end
      local window, err = rex.session.new_window{ layout = layout() }
      if err ~= nil then error(err, 0) end
      return window
    end,
  }
  if keys.open then rex.bind(keys.open, "swiftspot") end

  local transport = {
    { "toggle", "Play or Pause" },
    { "next", "Next Song" },
    { "previous", "Previous Song" },
    { "like", "Like Playing Song" },
  }
  for _, each in ipairs(transport) do
    local command, title = each[1], each[2]
    local name = "swiftspot_" .. command
    rex.action{
      name = name,
      title = title,
      category = "SwiftSpot",
      keywords = "spotify music swiftspot " .. command,
      run = function()
        local result, err = rex.exec({ bin, command }, { timeout = 10 })
        if result == nil then error(err, 0) end
        if not result.ok then error(result.stderr, 0) end
      end,
    }
    if keys[command] then rex.bind(keys[command], name) end
  end
end

return M
