// Development-only Kubernetes strategic patch oracle. Never linked into h3s.
package main

import (
	"encoding/json"
	"fmt"
	apps "k8s.io/api/apps/v1"
	core "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/util/strategicpatch"
	"os"
)

func main() {
	var cases []map[string]interface{}
	if err := json.NewDecoder(os.Stdin).Decode(&cases); err != nil {
		panic(err)
	}
	for _, c := range cases {
		delete(c, "error")
		delete(c, "expected")
		var typ interface{}
		switch c["kind"] {
		case "Service":
			typ = core.Service{}
		case "Node":
			typ = core.Node{}
		case "Pod":
			typ = core.Pod{}
		case "Deployment":
			typ = apps.Deployment{}
		case "ConfigMap":
			typ = core.ConfigMap{}
		case "ServiceAccount":
			typ = core.ServiceAccount{}
		default:
			panic("unknown kind")
		}
		old, _ := json.Marshal(c["original"])
		patch, _ := json.Marshal(c["patch"])
		result, err := strategicpatch.StrategicMergePatch(old, patch, typ)
		if err != nil {
			c["error"] = true
		} else {
			var value interface{}
			if err = json.Unmarshal(result, &value); err != nil {
				panic(err)
			}
			c["expected"] = value
		}
	}
	b, err := json.MarshalIndent(cases, "", "  ")
	if err != nil {
		panic(err)
	}
	fmt.Println(string(b))
}
