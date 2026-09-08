package kube

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/flannel-io/flannel/pkg/ip"
	"github.com/flannel-io/flannel/pkg/lease"
	"github.com/flannel-io/flannel/pkg/subnet"
	v1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes/fake"
)

// A freshly registered Kubernetes Node may legitimately omit annotations.
func TestAcquireLeaseAnnotations(t *testing.T) {
	for _, initial := range []map[string]string{nil, {}, {"example.test/preserve": "yes"}} {
		t.Run(func() string {
			if initial == nil {
				return "absent"
			}
			if len(initial) == 0 {
				return "empty"
			}
			return "populated"
		}(), func(t *testing.T) {
			node := &v1.Node{ObjectMeta: metav1.ObjectMeta{Name: "worker", Annotations: initial}, Spec: v1.NodeSpec{PodCIDR: "10.42.2.0/24", PodCIDRs: []string{"10.42.2.0/24"}}}
			client := fake.NewClientset(node)
			config, err := subnet.ParseConfig(`{"Network":"10.42.0.0/16","Backend":{"Type":"vxlan"}}`)
			if err != nil {
				t.Fatal(err)
			}
			names, err := newAnnotations("flannel.alpha.coreos.com")
			if err != nil {
				t.Fatal(err)
			}
			address, err := ip.ParseIP4("192.168.104.3")
			if err != nil {
				t.Fatal(err)
			}
			manager := &kubeSubnetManager{client: client, nodeName: "worker", disableNodeInformer: true, enableIPv4: true, annotations: names, subnetConf: config}
			acquired, err := manager.AcquireLease(context.Background(), &lease.LeaseAttrs{PublicIP: address, BackendType: "vxlan", BackendData: json.RawMessage(`{"VNI":1,"VtepMAC":"02:00:00:00:00:01"}`), BackendV6Data: json.RawMessage(`null`)})
			if err != nil {
				t.Fatal(err)
			}
			if acquired.Subnet.String() != "10.42.2.0/24" {
				t.Fatalf("wrong allocation: %s", acquired.Subnet.String())
			}
			updated, err := client.CoreV1().Nodes().Get(context.Background(), "worker", metav1.GetOptions{})
			if err != nil {
				t.Fatal(err)
			}
			if updated.Annotations[names.BackendType] != "vxlan" || updated.Annotations[names.BackendPublicIP] != "192.168.104.3" || updated.Annotations[names.SubnetKubeManaged] != "true" {
				t.Fatalf("missing backend metadata: %#v", updated.Annotations)
			}
			if initial != nil && initial["example.test/preserve"] == "yes" && updated.Annotations["example.test/preserve"] != "yes" {
				t.Fatal("lost existing annotation")
			}
		})
	}
}
