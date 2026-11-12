local logger     = require("logger")
local millennium = require("millennium")

-- steam-mcp writes a JSON request to config key "request:<id>" over Millennium's
-- socket. Hand it to the frontend, which runs it against Steam's collectionStore
-- and writes the answer to "response:<id>".
local REQUEST_PREFIX = "request:"

local function on_config_change(key, value)
    if type(key) ~= "string" or key:sub(1, #REQUEST_PREFIX) ~= REQUEST_PREFIX or type(value) ~= "string" then
        return
    end
    local id = key:sub(#REQUEST_PREFIX + 1)
    millennium.config.delete(key)

    local accepted, err = millennium.call_frontend_method("handleRequest", { id, value })
    if accepted ~= true then
        logger:error("handleRequest failed: " .. tostring(err))
        millennium.config.set("response:" .. id, {
            ok = false,
            error = "The Steam UI is not ready for steam-mcp-bridge: " .. tostring(err),
        })
    end
end

local function on_load()
    millennium.config.on_change(on_config_change)
    millennium.ready()
end

local function on_unload()
end

return {
    on_load = on_load,
    on_unload = on_unload,
}
