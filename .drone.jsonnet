local registry = "084037375216.dkr.ecr.us-east-2.amazonaws.com";

local targets = [
//      { "os": "linux",   "name": "sid", "isas": [ "386", "armv7", "amd64", "arm64", "mips64le", "ppc64le", "s390x", "riscv64" ], "events": [ "push", "tag", "custom" ] },
      { "os": "windows", distro: "windows", "name": "windows",  "isas": [ "x64" ], "events": [ "push", "tag", "custom" ] },
];

local Build(platform, distro, os, isa, events) = {
  "kind": "pipeline",
  "type": "docker",
  "pull": "always",
  "name": platform + " " + isa + " " + "build",
  "clone": {
    "image": "084037375216.dkr.ecr.us-east-2.amazonaws.com/drone-git",
    "depth": 1
  },
  "steps": [
    {
      "name": "build",
      [ if os == "linux" then "image" ]: registry + "/honda-builder",
      [ if os == "windows" then "image" ]: registry + "/windows-builder",
      [ if os == "linux" then "commands" ]: [ "./ci/scripts/build.sh " + platform + " " + isa + " " + "100.0.0+${DRONE_COMMIT_SHA:0:8}" + " " + "${DRONE_BUILD_EVENT}"  ],
      [ if os == "windows" then "commands" ]: [                      
        "Get-ChildItem 'C:\\Program Files (x86)\\Microsoft Visual Studio\\2022\\BuildTools\\MSBuild\\Microsoft\\VC'",
        "msbuild windows\\ZeroTierOne.sln /m /p:Configuration=Release  /property:Platform=x64 /t:ZeroTierOne:Rebuild",
      ]
    },
  ],
  "platform": {
     "os": os,
     [ if isa == "arm64" || isa == "armv7" then "arch" ]: "arm64",
  },
  "trigger": { "event": events }
};


std.flattenArrays([
    [
      Build(p.name, p.distro, p.os, isa, p.events)
        for isa in p.isas
    ]
    for p in targets
])
