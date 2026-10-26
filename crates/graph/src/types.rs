//! Typed subset of the Microsoft Graph `driveItem` resource.
//!
//! Only the fields the sync engine needs are deserialised; everything else in
//! the JSON is skipped by serde without being allocated.

use serde::Deserialize;
use serde::de::IgnoredAny;

/// `$select` used for every `/delta` request (same as the D client).
pub const DELTA_SELECT: &str = "id,name,eTag,cTag,deleted,file,folder,root,fileSystemInfo,remoteItem,parentReference,size,createdBy,lastModifiedBy,package";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    pub name: Option<String>,
    pub e_tag: Option<String>,
    pub c_tag: Option<String>,
    pub deleted: Option<IgnoredAny>,
    pub file: Option<FileFacet>,
    pub folder: Option<IgnoredAny>,
    pub root: Option<IgnoredAny>,
    pub file_system_info: Option<FileSystemInfo>,
    pub remote_item: Option<RemoteItem>,
    pub parent_reference: Option<ItemReference>,
    pub size: Option<i64>,
    pub package: Option<Package>,
    pub malware: Option<IgnoredAny>,
}

impl DriveItem {
    pub fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }
    pub fn is_root(&self) -> bool {
        self.root.is_some()
    }
    pub fn is_file(&self) -> bool {
        self.file.is_some()
    }
    pub fn is_folder(&self) -> bool {
        self.folder.is_some()
    }
    pub fn is_remote(&self) -> bool {
        self.remote_item.is_some()
    }
    pub fn is_malware(&self) -> bool {
        self.malware.is_some()
    }
    pub fn is_onenote_package(&self) -> bool {
        self.package.as_ref().and_then(|p| p.kind.as_deref()) == Some("oneNote")
    }
    pub fn parent_drive_id(&self) -> Option<&str> {
        self.parent_reference.as_ref()?.drive_id.as_deref()
    }
    pub fn parent_id(&self) -> Option<&str> {
        self.parent_reference.as_ref()?.id.as_deref()
    }
    pub fn quick_xor_hash(&self) -> Option<&str> {
        self.file.as_ref()?.hashes.as_ref()?.quick_xor_hash.as_deref()
    }
    pub fn sha256_hash(&self) -> Option<&str> {
        self.file.as_ref()?.hashes.as_ref()?.sha256_hash.as_deref()
    }
    pub fn last_modified(&self) -> Option<&str> {
        self.file_system_info.as_ref()?.last_modified_date_time.as_deref()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    pub hashes: Option<Hashes>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    pub quick_xor_hash: Option<String>,
    pub sha256_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemInfo {
    pub last_modified_date_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemReference {
    pub drive_id: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteItem {
    pub id: Option<String>,
    pub parent_reference: Option<ItemReference>,
    pub file_system_info: Option<FileSystemInfo>,
    pub folder: Option<IgnoredAny>,
    pub file: Option<IgnoredAny>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// One page of a `/delta` response.
#[derive(Debug, Deserialize)]
pub struct DeltaPage {
    pub value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    pub drive_type: Option<String>,
    pub owner: Option<IdentitySet>,
    pub quota: Option<Quota>,
}

impl Drive {
    pub fn is_personal(&self) -> bool {
        self.drive_type.as_deref() == Some("personal")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentitySet {
    pub user: Option<Identity>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub display_name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    pub total: Option<i64>,
    pub used: Option<i64>,
    pub remaining: Option<i64>,
}

/// A SharePoint site (`/sites?search=`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteResource {
    pub id: String,
    pub display_name: Option<String>,
    pub name: Option<String>,
    pub web_url: Option<String>,
}

/// A drive as listed by `/sites/{id}/drives`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveListing {
    pub id: String,
    pub name: Option<String>,
    pub drive_type: Option<String>,
    pub web_url: Option<String>,
}

/// A child of a folder, as listed by `/children` (only what a folder chooser needs).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildListing {
    pub name: String,
    pub folder: Option<FolderFacet>,
    pub package: Option<Package>,
    pub remote_item: Option<IgnoredAny>,
}

impl ChildListing {
    /// A plain folder (not a OneNote notebook nor a shortcut to a shared folder).
    pub fn is_folder(&self) -> bool {
        self.folder.is_some() && self.package.is_none() && self.remote_item.is_none()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderFacet {
    pub child_count: Option<i64>,
}

/// A page of an OData collection.
#[derive(Debug, Deserialize)]
pub struct Page<T> {
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_delta_page() {
        let json = r#"{
          "@odata.nextLink": "https://graph.microsoft.com/next",
          "value": [
            {"id": "R", "name": "root", "root": {}, "folder": {"childCount": 2},
             "fileSystemInfo": {"lastModifiedDateTime": "2026-10-21T09:04:48Z"},
             "parentReference": {"driveId": "D", "driveType": "business"}},
            {"id": "F", "name": "nut.html", "size": 9795, "eTag": "e", "cTag": "c",
             "file": {"mimeType": "text/html", "hashes": {"quickXorHash": "vjx4FqWxLHLoaBqk/jVKY97EZ7k="}},
             "fileSystemInfo": {"createdDateTime": "2019-03-05T03:15:20Z", "lastModifiedDateTime": "2019-03-05T03:15:20Z"},
             "parentReference": {"driveId": "D", "id": "R", "path": "/drive/root:"},
             "createdBy": {"user": {"displayName": "x"}}},
            {"id": "X", "deleted": {"state": "deleted"}, "parentReference": {"driveId": "D", "id": "R"}}
          ]
        }"#;
        let page: DeltaPage = serde_json::from_str(json).unwrap();
        assert_eq!(page.value.len(), 3);
        assert!(page.value[0].is_root() && page.value[0].is_folder());
        assert_eq!(page.value[1].quick_xor_hash(), Some("vjx4FqWxLHLoaBqk/jVKY97EZ7k="));
        assert_eq!(page.value[1].parent_id(), Some("R"));
        assert!(page.value[2].is_deleted() && page.value[2].name.is_none());
        assert!(page.next_link.is_some() && page.delta_link.is_none());
    }
}
