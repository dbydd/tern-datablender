--!strict

local field_extensions = { nc = true, nc4 = true, cdf = true, mat = true, h5 = true, hdf5 = true }

local function quote(text: string): string
    return "'" .. text:gsub("'", "'\\''") .. "'"
end

local function command(binary: string, project: string, path: string?): string
    if tern.runtime.os == "windows" then
        local function ps_quote(value: string): string
            return "'" .. value:gsub("'", "''") .. "'"
        end
        local script = "& " .. ps_quote(binary) .. " view --project " .. ps_quote(project)
        if path then script ..= " -- " .. ps_quote(path) end
        local bytes = {}
        for _, code in utf8.codes(script) do
            if code > 65535 then
                code -= 65536
                local high = 55296 + math.floor(code / 1024)
                local low = 56320 + code % 1024
                table.insert(bytes, string.char(high % 256, math.floor(high / 256), low % 256, math.floor(low / 256)))
            else
                table.insert(bytes, string.char(code % 256, math.floor(code / 256)))
            end
        end
        return "powershell.exe -NoProfile -EncodedCommand " .. tern.base64.encode(table.concat(bytes))
    end
    local line = quote(binary) .. " view --project " .. quote(project)
    if path then line ..= " -- " .. quote(path) end
    return line
end

local function launch(cx: any, path: string?, project: string?, how: string?): boolean
    if cx.hosts:current().kind ~= "local" then
        cx:toast("error", "DataBlender opens files on the local host")
        return false
    end
    local suffix = if tern.runtime.os == "windows" then ".exe" else ""
    local binary = tern.plugin.dir .. "/bin/tern-datablender" .. suffix
    if not tern.fs.exists(binary) then
        binary = tern.plugin.dir .. "/target/release/tern-datablender" .. suffix
    end
    if not tern.fs.exists(binary) then
        binary = tern.plugin.dir .. "/target/debug/tern-datablender" .. suffix
    end
    if not tern.fs.exists(binary) then
        cx:toast("error", "DataBlender binary is missing", "Install the release package for this platform, or build the source checkout.")
        return false
    end
    if not project then
        local serial = (tern.kv.get("project_serial") or 0) + 1
        tern.kv.set("project_serial", serial)
        tern.fs.mkdir(tern.plugin.data .. "/projects")
        project = tern.plugin.data .. "/projects/project-" .. tostring(serial) .. ".json"
    end
    local focused = cx.session:focused()
    local spec = { command = command(binary, project, path) }
    local pane
    if focused and (how == "beside" or how == "split" or how == "below") then
        pane = cx.layout:split(focused, if how == "below" then "down" else "right", spec)
    else
        pane = cx.layout:new_tab(spec)
    end
    if not pane then
        cx:toast("error", "DataBlender could not open a pane")
        return false
    end
    tern.kv.set("last_project", project)
    return true
end

local function extension(path: string): string
    return (path:lower():match("%.([^./\\]+)$"))
end

tern.command({
    id = "open", title = "DataBlender: Open data file", icon = "file",
    run = function(cx: any)
        cx:prompt({ placeholder = "Data path (.nc, .nc4, .cdf, .mat, .h5, .hdf5)" }):next(function(path: string?, err: string?, fresh: any)
            if err then fresh:toast("error", err) return end
            if path and path ~= "" then launch(fresh, path, nil, "tab") end
        end)
    end,
})

tern.command({
    id = "new", title = "DataBlender: New workspace", icon = "file",
    run = function(cx: any) launch(cx, nil, nil, "tab") end,
})

tern.command({
    id = "resume", title = "DataBlender: Resume last project", icon = "file",
    available = function(_cx: any) return tern.kv.get("last_project") ~= nil end,
    run = function(cx: any)
        local project = tern.kv.get("last_project")
        if project then launch(cx, nil, project, "tab") end
    end,
})

tern.route.open(function(req: any, cx: any)
    if cx.hosts:current().kind ~= "local" or not tern.fs.exists(req.path) then return nil end
    if not field_extensions[extension(req.path)] then return nil end
    if launch(cx, req.path, nil, req.how) then return { handled = true } end
    return nil
end)
