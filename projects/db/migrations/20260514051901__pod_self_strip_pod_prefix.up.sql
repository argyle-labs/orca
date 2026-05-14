-- Strip the legacy "pod-" prefix from pod_self.pod_id. Earlier `pod init`
-- wrote ids of the form "pod-XXXXXXXX", which then rendered in mDNS as
-- "pod:pod-XXXXXXXX". New ids are just the 8-char uuid slice.
UPDATE pod_self SET pod_id = substr(pod_id, 5)
WHERE pod_id LIKE 'pod-%';
